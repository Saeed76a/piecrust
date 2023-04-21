# π-crust

[![Repository](https://img.shields.io/badge/github-piecrust-blueviolet?logo=github)](https://github.com/dusk-network/piecrust)
![Build Status](https://github.com/dusk-network/piecrust/workflows/build/badge.svg)
[![Documentation](https://img.shields.io/badge/docs-piecrust-blue?logo=rust)](https://docs.rs/piecrust/)

`piecrust` is a Rust workspace containing two crates, `piecrust` and `piecrust-uplink`, that together form the WASM virtual machine for running, handling and creating Dusk smart contracts.

## Workspace members

- [piecrust](piecrust/README.md): WASM virtual machine for running Dusk's smart contracts.
- [piecrust-uplink](piecrust-uplink/README.md): The library that allows you to create smart contracts directly on top of `piecrust`.

## Project structure

The project is organized as follows:

- `modules`: Contains a number of example smart contracts that can be ran against the `piecrust` virtual machine.
- `piecrust`: Contains the source code and README for the WASM virtual machine.
- `piecrust-uplink`: Contains the source code and README for the smart contract development kit.

## Usage

```rust
use piecrust::VM;
let mut vm = VM::ephemeral().unwrap();

let bytecode = /*load bytecode*/;

let mut session = vm.session(SessionData::builder())?;
let contract_id = session.deploy(bytecode).unwrap();

let result = session.transact::<i16, i32>(contract_id, "function_name", &0x11)?;

// use result
```

## Build and Test

To build and test the crate one will need a
[Rust](https://www.rust-lang.org/tools/install) toolchain, Make, and the
`wasm-tools` binary.

```sh
sudo apt install -y make # ubuntu/debian - adapt to own system
cargo install wasm-tools
make test
```
