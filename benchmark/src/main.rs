use async_trait::async_trait;
use hbbft::honey_badger::{HoneyBadger, Step};
use hbbft::{Epoched, Message, NetworkInfo, Target};
use itertools::Itertools;
use minimint::config::{ClientConfig, ServerConfig, ServerConfigParams};
use minimint::consensus::{ConsensusItem, ConsensusOutcome, FediMintConsensus};
use minimint::modules::ln::LightningModule;
use minimint::modules::mint::tiered::coins::Coins;
use minimint::modules::mint::{Coin, CoinNonce, EventType, LogEvent};
use minimint::net;
use minimint::net::connect::Connections;
use minimint::net::PeerConnections;
use minimint::rng::RngGenerator;
use minimint_api::config::GenerateConfig;
use minimint_api::db::mem_impl::MemDatabase;
use minimint_api::db::Database;
use minimint_api::{Amount, OutPoint, PeerId};
use mint_client::mint::{MintClient, MintClientError, SpendableCoin};
use mint_client::UserClient;
use rand::{thread_rng, CryptoRng, RngCore};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::Duration;
use structopt::StructOpt;
use tbs::{blind_message, combine_valid_shares, sign_blinded_msg, unblind_signature};
use tempdir::TempDir;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::task::{spawn, JoinHandle};
use tokio::time::Instant;
use tracing::{debug, info, trace, warn};
use tracing_subscriber::EnvFilter;

type ConsensusMessage = hbbft::honey_badger::Message<PeerId>;

const DENOMINATION: Amount = Amount::from_msat(1);

/// Start all the components of the mint and plug them together
pub async fn run_minimint_node(
    cfg: ServerConfig,
    network: LatencyNetwork,
    database: Arc<dyn Database>,
    submit_tx: tokio::sync::broadcast::Receiver<minimint::transaction::Transaction>,
    log_events: tokio::sync::mpsc::Sender<LogEvent>,
) {
    assert_eq!(
        cfg.peers.keys().max().copied().map(|id| id.to_usize()),
        Some(cfg.peers.len() - 1)
    );
    assert_eq!(cfg.peers.keys().min().copied(), Some(PeerId::from(0)));

    let threshold = cfg.peers.len() - cfg.max_faulty();

    let mint = minimint::modules::mint::Mint::new(
        cfg.mint.clone(),
        threshold,
        database.clone(),
        log_events,
    );
    let wallet = minimint::modules::wallet::Wallet::new(cfg.wallet.clone(), database.clone())
        .await
        .expect("Couldn't create wallet");
    let ln = LightningModule::new(cfg.ln.clone(), database.clone());

    let mint_consensus = Arc::new(FediMintConsensus {
        rng_gen: Box::new(CloneRngGen(Mutex::new(rand::rngs::OsRng::new().unwrap()))), //FIXME
        cfg: cfg.clone(),
        mint,
        wallet,
        ln,
        db: database,
    });

    let submit_mint = mint_consensus.clone();
    spawn(async move {
        let mut submit_tx = submit_tx;
        while let Ok(tx) = submit_tx.recv().await {
            submit_mint.submit_transaction(tx).unwrap();
        }
    });

    let (output_sender, mut output_receiver) = channel::<ConsensusOutcome>(1);
    let (proposal_sender, proposal_receiver) = channel::<Vec<ConsensusItem>>(1);

    info!("Spawning consensus with first proposal");
    spawn_hbbft(
        output_sender,
        proposal_receiver,
        cfg.clone(),
        mint_consensus.get_consensus_proposal().await,
        network,
        rand::rngs::OsRng::new().unwrap(),
    )
    .await;

    // FIXME: reusing the wallet CI leads to duplicate randomness beacons, not a problem for change, but maybe later for other use cases
    debug!("Generating second proposal");
    let mut proposal = Some(mint_consensus.get_consensus_proposal().await);
    loop {
        debug!("Ready to exchange proposal for consensus outcome");

        // We filter out the already agreed on consensus items from our proposal to avoid proposing
        // duplicates. Yet we can not remove them from the database entirely because we might crash
        // while processing the outcome.
        let outcome = {
            let outcome = output_receiver.recv().await.expect("other thread died");
            debug!(
                "Received outcome containing {} items from {} peers for epoch {}",
                outcome
                    .contributions
                    .values()
                    .map(|c| c.len())
                    .sum::<usize>(),
                outcome.contributions.len(),
                outcome.epoch
            );
            let outcome_filter_set = outcome
                .contributions
                .values()
                .flatten()
                .filter(|ci| !matches!(ci, ConsensusItem::Wallet(_)))
                .collect::<HashSet<_>>();
            debug!("Constructing filter set");

            let full_proposal = proposal.take().expect("Is always refilled");
            let filtered_proposal = full_proposal
                .into_iter()
                .filter(|ci| !outcome_filter_set.contains(ci))
                .collect::<Vec<ConsensusItem>>();
            debug!("Filtered proposal");
            let proposal_len = filtered_proposal.len();
            proposal_sender
                .send(filtered_proposal)
                .await
                .expect("other thread died");
            debug!("Sent new proposal containing {} items", proposal_len);

            outcome
        };
        debug!(
            "Processing consensus outcome from epoch {} with {} items",
            outcome.epoch,
            outcome.contributions.values().flatten().count()
        );
        mint_consensus.process_consensus_outcome(outcome).await;
        proposal = Some(mint_consensus.get_consensus_proposal().await);
    }
}

async fn spawn_hbbft(
    outcome_sender: Sender<ConsensusOutcome>,
    mut proposal_receiver: Receiver<Vec<ConsensusItem>>,
    cfg: ServerConfig,
    initial_cis: Vec<ConsensusItem>,
    mut connections: LatencyNetwork,
    mut rng: impl RngCore + CryptoRng + Clone + Send + 'static,
) -> JoinHandle<()> {
    spawn(async move {
        let net_info = NetworkInfo::new(
            cfg.identity,
            cfg.hbbft_sks.inner().clone(),
            cfg.hbbft_pk_set.clone(),
            cfg.hbbft_sk.inner().clone(),
            cfg.peers
                .iter()
                .map(|(id, peer)| (*id, peer.hbbft_pk))
                .collect(),
        );

        let mut hb: HoneyBadger<Vec<ConsensusItem>, _> =
            HoneyBadger::builder(Arc::new(net_info)).build();
        info!("Created Honey Badger instance");

        let mut next_consensus_items = Some(initial_cis);
        loop {
            let contribution = next_consensus_items
                .take()
                .expect("This is always refilled");

            debug!(
                "Proposing a contribution with {} consensus items for epoch {}",
                contribution.len(),
                hb.epoch()
            );
            trace!("Contribution: {:?}", contribution);
            let mut initial_step = Some(
                hb.propose(&contribution, &mut rng)
                    .expect("Failed to process HBBFT input"),
            );

            let outcome = 'inner: loop {
                // We either want to handle the initial step or generate a new one by receiving a
                // message from a peer
                let Step {
                    output,
                    fault_log,
                    messages,
                } = match initial_step.take() {
                    Some(step) => step,
                    None => {
                        let (peer, peer_msg) = connections.receive().await;
                        trace!("Received message from {}", peer);
                        hb.handle_message(&peer, peer_msg)
                            .expect("Failed to process HBBFT input")
                    }
                };

                for msg in messages {
                    trace!("sending message to {:?}", msg.target);
                    connections.send(msg.target, msg.message).await;
                }

                if !fault_log.is_empty() {
                    warn!("Faults: {:?}", fault_log);
                }

                if !output.is_empty() {
                    trace!("Processed step had an output, handing it off");
                    break 'inner output;
                }
            };

            debug!("received {} batches", outcome.len());
            for batch in outcome {
                debug!("Exchanging consensus outcome of epoch {}", batch.epoch);
                // Old consensus contributions are overwritten on case of multiple batches arriving
                // at once. The new contribution should be used to avoid redundantly included items.
                outcome_sender.send(batch).await.expect("other thread died");
                debug!("Awaiting proposal for next epoch");
                next_consensus_items =
                    Some(proposal_receiver.recv().await.expect("other thread died"));
            }
        }
    })
}

struct CloneRngGen<T: RngCore + CryptoRng + Clone + Send>(Mutex<T>);

impl<T: RngCore + CryptoRng + Clone + Send> RngGenerator for CloneRngGen<T> {
    type Rng = T;

    fn get_rng(&self) -> Self::Rng {
        self.0.lock().unwrap().clone()
    }
}

pub struct LatencyNetwork {
    id: PeerId,
    offline_peers: Vec<PeerId>,
    latency: Duration,
    peers:
        BTreeMap<PeerId, tokio::sync::mpsc::UnboundedSender<(PeerId, Instant, ConsensusMessage)>>,
    receive: tokio::sync::mpsc::UnboundedReceiver<(PeerId, Instant, ConsensusMessage)>,
}

#[async_trait]
impl PeerConnections<ConsensusMessage> for LatencyNetwork {
    type Id = PeerId;

    async fn send(&mut self, target: Target<Self::Id>, msg: ConsensusMessage) {
        let delivery_at = Instant::now() + self.latency;
        trace!("Sending msg from {} to {:?}", self.id, target);
        match target {
            Target::All => {
                for (id, peer_conn) in self.peers.iter_mut() {
                    if !self.offline_peers.contains(&id) {
                        peer_conn.send((self.id, delivery_at, msg.clone())).unwrap();
                    }
                }
            }
            Target::Node(peer) => {
                if !self.offline_peers.contains(&peer) {
                    self.peers[&peer].send((self.id, delivery_at, msg)).unwrap();
                }
            }
        }
    }

    async fn receive(&mut self) -> (Self::Id, ConsensusMessage) {
        let (sender, delivery_at, msg) = self.receive.recv().await.unwrap();
        tokio::time::sleep_until(delivery_at).await;
        trace!("Peer {} received msg from {}", self.id, sender);
        (sender, msg)
    }
}

fn start_latency_network(
    latency: Duration,
    nodes: &[PeerId],
    offline_peers: Vec<PeerId>,
) -> BTreeMap<PeerId, LatencyNetwork> {
    let (senders, receivers): (BTreeMap<_, _>, Vec<_>) = nodes
        .iter()
        .map(|&peer| {
            let (send, receive) = tokio::sync::mpsc::unbounded_channel();
            ((peer, send), (peer, receive))
        })
        .unzip();

    receivers
        .into_iter()
        .map(|(id, receiver)| {
            let net = LatencyNetwork {
                id,
                offline_peers: offline_peers.clone(),
                latency,
                peers: senders
                    .iter()
                    .filter_map(|(&peer_id, sender)| {
                        if peer_id != id {
                            Some((peer_id, sender.clone()))
                        } else {
                            None
                        }
                    })
                    .collect(),
                receive: receiver,
            };
            (id, net)
        })
        .collect()
}

fn generate_cfg(peers: &[PeerId]) -> (BTreeMap<PeerId, ServerConfig>, ClientConfig) {
    let max_evil = hbbft::util::max_faulty(peers.len());

    let params = ServerConfigParams {
        hbbft_base_port: 4000,
        api_base_port: 5000,
        amount_tiers: vec![DENOMINATION],
    };

    ServerConfig::trusted_dealer_gen(peers, max_evil, &params, thread_rng())
}

fn free_money(cfg: &BTreeMap<PeerId, ServerConfig>, amt: usize) -> Coins<SpendableCoin> {
    let threshold = cfg.len() - hbbft::util::max_faulty(cfg.len());
    (0..amt)
        .into_par_iter()
        .map(|_| {
            let mut rng = thread_rng();
            let (sk, pk) = secp256k1_zkp::SECP256K1.generate_schnorrsig_keypair(&mut rng);
            let nonce = CoinNonce(pk);
            let (bkey, bnonce) = blind_message(nonce.to_message());
            let bsig_shares = cfg
                .iter()
                .map(|(peer, cfg)| {
                    let bsig_share = sign_blinded_msg(bnonce, cfg.mint.tbs_sks.keys[&DENOMINATION]);
                    (peer.to_usize(), bsig_share)
                })
                .collect::<Vec<_>>();
            let bsig = combine_valid_shares(bsig_shares, threshold);
            let sig = unblind_signature(bkey, bsig);
            let coin = Coin(nonce, sig);
            (
                DENOMINATION,
                SpendableCoin {
                    coin,
                    spend_key: sk.serialize_secret(),
                },
            )
        })
        .collect::<Vec<_>>()
        .into_iter()
        .collect()
}

#[derive(StructOpt)]
struct Args {
    num_peers: u16,
    offline_peers: usize,
    latency_ms: u64,
    coins_per_tx: usize,
    tx_per_sec: f64,
    num_tx: usize,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args: Args = StructOpt::from_args();

    let peers = (0..args.num_peers).map(PeerId::from).collect::<Vec<_>>();
    let offline_peers = peers
        .iter()
        .rev()
        .take(args.offline_peers)
        .copied()
        .collect::<Vec<_>>();
    let latency = Duration::from_millis(args.latency_ms);
    let num_tx = args.num_tx;
    let coins_per_tx = args.coins_per_tx;
    let tx_per_sec = args.tx_per_sec;

    let coins_to_issue = num_tx * coins_per_tx;
    let num_peers = peers.len();
    let max_faulty = hbbft::util::max_faulty(num_peers);

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,tide=error")),
        )
        .init();

    let mut rng = thread_rng();
    let (mut server_cfg, client_cfg) = generate_cfg(&peers);
    let mut network = start_latency_network(latency, &peers, offline_peers.clone());
    debug!("Isuing {} coins", coins_to_issue);
    let free_money = free_money(&server_cfg, coins_to_issue);
    debug!("Done!");
    let tmp_dir = TempDir::new("minimint-bench").unwrap();
    let databases: BTreeMap<_, Arc<dyn Database>> = peers
        .iter()
        .map(|&peer| {
            let db: Arc<dyn Database> = Arc::new(
                sled::open(tmp_dir.path().join(format!("peer-{}", peer)))
                    .unwrap()
                    .open_tree("default")
                    .unwrap(),
            );
            (peer, db)
        })
        .collect();
    let (tx_sender, _tx_receiver) = tokio::sync::broadcast::channel(1024);

    let (event_send, mut event_recv) = tokio::sync::mpsc::channel::<LogEvent>(1024);
    let log_handle = tokio::spawn(async move {
        let mut logs = Vec::with_capacity(3 * num_peers * num_tx);
        loop {
            if let Some(log) = event_recv.recv().await {
                logs.push(log);
            }

            // 1 submit + n accept/coins
            if logs.len() == (num_tx * (1 + 2 * (num_peers - args.offline_peers))) {
                break;
            }
        }
        logs
    });

    let fed = peers
        .iter()
        .filter(|peer| !offline_peers.contains(*peer))
        .map(|peer| {
            tokio::spawn(run_minimint_node(
                server_cfg.remove(peer).unwrap(),
                network.remove(peer).unwrap(),
                databases[peer].clone(),
                tx_sender.subscribe(),
                event_send.clone(),
            ))
        })
        .collect::<Vec<_>>();

    let client_db: Box<dyn Database> = Box::new(MemDatabase::new());
    let mint_client = UserClient::new(client_cfg, client_db, secp256k1_zkp::Secp256k1::new());
    mint_client.save_coins(free_money);

    debug!(
        "Sending {} tx per second with {} coins each, should take {}s",
        tx_per_sec,
        coins_per_tx,
        coins_to_issue as f64 / coins_per_tx as f64 / tx_per_sec
    );

    let mut interval = tokio::time::interval(Duration::from_secs_f64(1.0 / tx_per_sec));
    while let _ = interval.tick().await {
        let spend_coins =
            match mint_client.select_and_spend_coins(DENOMINATION * coins_per_tx as u64) {
                Ok(coins) => coins,
                Err(_) => {
                    break;
                }
            };
        let tx = mint_client.reissue(spend_coins, &mut rng).await.unwrap();
        let txid = tx.tx_hash();
        tx_sender.send(tx).unwrap();
        event_send
            .send(LogEvent {
                out_point: OutPoint { txid, out_idx: 0 },
                peer: PeerId::from(0xffff),
                action: EventType::Submitted,
                time: Instant::now(),
            })
            .await
            .unwrap();
        debug!("Submitted tx");
    }

    debug!("Done! Shutting down …");
    let log = log_handle.await.unwrap();
    debug!("Collected {} logs", log.len());

    for member in fed {
        member.abort();
        let _ = member.await;
    }

    debug!("Shutdown complete!");

    #[derive(Debug)]
    struct TransactionMetrics {
        accept_latency: Duration,
        coin_latency: Duration,
    }

    let transaction_logs = log.into_iter().into_group_map_by(|log| log.out_point);
    let transaction_metrics = transaction_logs
        .into_iter()
        .map(|(out_point, logs)| {
            let submit_time = logs
                .iter()
                .find_map(|log| {
                    if log.action == EventType::Submitted {
                        Some(log.time)
                    } else {
                        None
                    }
                })
                .unwrap();

            let mut accept_latencies = logs
                .iter()
                .filter_map(|log| {
                    if log.action == EventType::Agreed {
                        Some(log.time - submit_time)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            accept_latencies.sort();
            let accept_latency = accept_latencies[peers.len() - max_faulty - 1];

            let coin_latency = logs
                .iter()
                .filter_map(|log| {
                    if log.action == EventType::Final {
                        Some(log.time - submit_time)
                    } else {
                        None
                    }
                })
                .min()
                .unwrap();

            TransactionMetrics {
                accept_latency,
                coin_latency,
            }
        })
        .collect::<Vec<_>>();

    let accept_latencies = transaction_metrics
        .iter()
        .map(|m| m.accept_latency.as_secs_f64())
        .collect::<Vec<_>>();
    let coin_latencies = transaction_metrics
        .iter()
        .map(|m| m.coin_latency.as_secs_f64())
        .collect::<Vec<_>>();
    let mean_accept = statistical::mean(&accept_latencies);
    let mean_coin = statistical::mean(&coin_latencies);
    let stddev_accept = statistical::standard_deviation(&accept_latencies, None);
    let stddev_coin = statistical::standard_deviation(&coin_latencies, None);
    let max_accept = *accept_latencies
        .iter()
        .max_by(|a, b| a.partial_cmp(b).unwrap())
        .unwrap();
    let max_coins = *coin_latencies
        .iter()
        .max_by(|a, b| a.partial_cmp(b).unwrap())
        .unwrap();

    println!(
        "Accept: {}s +- {}s, max {}s",
        mean_accept, stddev_accept, max_accept
    );
    println!(
        "Coins:  {}s +- {}s, max {}s",
        mean_coin, stddev_coin, max_coins
    );
}
