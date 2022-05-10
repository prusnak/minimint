# MiniMint E-Cash Benchmarking Tool

The benchmarking tool simulates a federation in-process consisting of `<num-peers>` members of which `<offline-peers>` are offline. They are connected via a virtual network with a latency of `<latency-ms>` ms. The benchmark sends `<num-tx>` transactions with `<coins-per-tx>` coins per transactions at `<tx-per-sec>` transactions per second.

It outputs a CSV line containing the mean time till transaction acceptance, the standard deviation and the maximum thereof and the same for the time till e-cash tokens are actually received.

The tool can be run using an upt-to-date rust toolchain using cargo: `cargo run --release --bin benchmark <num-peers> <offline-peers> <latency-ms> <coins-per-tx> <tx-per-sec> <num-tx>`.

```
benchmark 0.1.0

USAGE:
    benchmark <num-peers> <offline-peers> <latency-ms> <coins-per-tx> <tx-per-sec> <num-tx>

FLAGS:
    -h, --help       Prints help information
    -V, --version    Prints version information

ARGS:
    <num-peers>        
    <offline-peers>    
    <latency-ms>       
    <coins-per-tx>     
    <tx-per-sec>       
    <num-tx>           
```