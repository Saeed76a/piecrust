// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.
//
// Copyright (c) DUSK NETWORK. All rights reserved.

use piecrust::{module_bytecode, Error, VM};
use rkyv::Deserialize;

fn hash(buf: &mut [u8], len: u32) -> u32 {
    let a = unsafe { rkyv::archived_root::<Vec<u8>>(&buf[..len as usize]) };
    let v: Vec<u8> = a.deserialize(&mut rkyv::Infallible).unwrap();

    let hash = blake3::hash(&v);
    buf[..32].copy_from_slice(&hash.as_bytes()[..]);

    32
}

#[test]
pub fn host_hash() -> Result<(), Error> {
    let mut vm = VM::ephemeral()?;

    let mut session = vm.session();

    let id = session.deploy(module_bytecode!("host"))?;

    vm.register_host_query("hash", hash);

    let v = vec![0u8, 1, 2];
    let h = session
        .query::<_, [u8; 32]>(id, "hash", v)
        .expect("query should succeed");
    assert_eq!(blake3::hash(&vec![0u8, 1, 2]).as_bytes(), &h);

    Ok(())
}
