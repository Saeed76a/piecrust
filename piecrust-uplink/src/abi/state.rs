// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.
//
// Copyright (c) DUSK NETWORK. All rights reserved.

use alloc::vec::Vec;

use rkyv::{
    archived_root,
    ser::serializers::{BufferScratch, BufferSerializer, CompositeSerializer},
    ser::Serializer,
    Archive, Archived, Deserialize, Infallible, Serialize,
};

use crate::{
    ContractError, ContractId, StandardBufSerializer, CONTRACT_ID_BYTES,
    SCRATCH_BUF_BYTES,
};

pub mod arg_buf {
    use crate::ARGBUF_LEN;

    #[no_mangle]
    static mut A: [u64; ARGBUF_LEN / 8] = [0; ARGBUF_LEN / 8];

    pub fn with_arg_buf<F, R>(f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let buf = unsafe { &mut A };
        let first = &mut buf[0];
        let slice = unsafe {
            let first_byte: &mut u8 = core::mem::transmute(first);
            core::slice::from_raw_parts_mut(first_byte, ARGBUF_LEN)
        };

        f(slice)
    }
}

pub(crate) use arg_buf::with_arg_buf;

mod ext {
    extern "C" {
        pub fn hq(name: *const u8, name_len: u32, arg_len: u32) -> u32;
        pub fn hd(name: *const u8, name_len: u32) -> u32;

        pub fn c(
            contract_id: *const u8,
            fn_name: *const u8,
            fn_name_len: u32,
            fn_arg_len: u32,
            points_limit: u64,
        ) -> i32;

        pub fn emit(topic: *const u8, topic_len: u32, arg_len: u32);
        pub fn feed(arg_len: u32);

        pub fn caller();
        pub fn limit() -> u64;
        pub fn spent() -> u64;
        pub fn owner();
        pub fn self_id();
    }
}

/// Execute some code that the host provides under the given name.
pub fn host_query<A, Ret>(name: &str, arg: A) -> Ret
where
    A: for<'a> Serialize<StandardBufSerializer<'a>>,
    Ret: Archive,
    Ret::Archived: Deserialize<Ret, Infallible>,
{
    let arg_len = with_arg_buf(|buf| {
        let mut sbuf = [0u8; SCRATCH_BUF_BYTES];
        let scratch = BufferScratch::new(&mut sbuf);
        let ser = BufferSerializer::new(buf);
        let mut composite =
            CompositeSerializer::new(ser, scratch, rkyv::Infallible);
        composite.serialize_value(&arg).expect("infallible");
        composite.pos() as u32
    });

    let name_ptr = name.as_bytes().as_ptr();
    let name_len = name.as_bytes().len() as u32;

    let ret_len = unsafe { ext::hq(name_ptr, name_len, arg_len) };

    with_arg_buf(|buf| {
        let slice = &buf[..ret_len as usize];
        let ret = unsafe { archived_root::<Ret>(slice) };
        ret.deserialize(&mut Infallible).expect("Infallible")
    })
}

/// Calls a `contract`'s `fn_name` function with the given argument `fn_arg`.
/// The contract will have `93%` of the remaining points available to spend.
///
/// To specify the points allowed to be spent by the called contract, use
/// [`call_with_limit`].
pub fn call<A, Ret>(
    contract: ContractId,
    fn_name: &str,
    fn_arg: &A,
) -> Result<Ret, ContractError>
where
    A: for<'a> Serialize<StandardBufSerializer<'a>>,
    Ret: Archive,
    Ret::Archived: Deserialize<Ret, Infallible>,
{
    call_with_limit(contract, fn_name, fn_arg, 0)
}

/// Calls a `contract`'s `fn_name` function with the given argument `fn_arg`,
/// allowing it to spend the given `points_limit`.
///
/// A points limit of `0` is equivalent to using [`call`], and will use the
/// default behavior - i.e. the called contract gets `93%` of the remaining
/// points.
///
/// If the points limit given is above or equal the remaining amount, the
/// default behavior will be used instead.
pub fn call_with_limit<A, Ret>(
    contract: ContractId,
    fn_name: &str,
    fn_arg: &A,
    points_limit: u64,
) -> Result<Ret, ContractError>
where
    A: for<'a> Serialize<StandardBufSerializer<'a>>,
    Ret: Archive,
    Ret::Archived: Deserialize<Ret, Infallible>,
{
    let arg_len = with_arg_buf(|buf| {
        let mut sbuf = [0u8; SCRATCH_BUF_BYTES];
        let scratch = BufferScratch::new(&mut sbuf);
        let ser = BufferSerializer::new(buf);
        let mut composite =
            CompositeSerializer::new(ser, scratch, rkyv::Infallible);
        composite.serialize_value(fn_arg).expect("infallible");
        composite.pos() as u32
    });

    let fn_name = fn_name.as_bytes();

    let ret_len = unsafe {
        ext::c(
            &contract.as_bytes()[0],
            fn_name.as_ptr(),
            fn_name.len() as u32,
            arg_len,
            points_limit,
        )
    };

    with_arg_buf(|buf| {
        if ret_len < 0 {
            Err(ContractError::from_parts(ret_len, buf))
        } else {
            let slice = &buf[..ret_len as usize];
            let ret = unsafe { archived_root::<Ret>(slice) };
            Ok(ret.deserialize(&mut Infallible).expect("Infallible"))
        }
    })
}

/// Calls the function with name `fn_name` of the given `contract` using
/// `fn_arg` as argument.
///
/// To specify the points allowed to be spent by the called contract, use
/// [`call_raw_with_limit`].
pub fn call_raw(
    contract: ContractId,
    fn_name: &str,
    fn_arg: &[u8],
) -> Result<Vec<u8>, ContractError> {
    call_raw_with_limit(contract, fn_name, fn_arg, 0)
}

/// Calls the function with name `fn_name` of the given `contract` using
/// `fn_arg` as argument, allowing it to spend the given `points_limit`.
///
/// A point limit of `0` is equivalent to using [`call_raw`], and will use the
/// default behavior - i.e. the called contract gets `93%` of the remaining
/// points.
///
/// If the points limit given is above or equal the remaining amount, the
/// default behavior will be used instead.
pub fn call_raw_with_limit(
    contract: ContractId,
    fn_name: &str,
    fn_arg: &[u8],
    points_limit: u64,
) -> Result<Vec<u8>, ContractError> {
    with_arg_buf(|buf| {
        buf[..fn_arg.len()].copy_from_slice(fn_arg);
    });

    let fn_name = fn_name.as_bytes();

    let ret_len = unsafe {
        ext::c(
            &contract.as_bytes()[0],
            fn_name.as_ptr(),
            fn_name.len() as u32,
            fn_arg.len() as u32,
            points_limit,
        )
    };

    with_arg_buf(|buf| {
        if ret_len < 0 {
            Err(ContractError::from_parts(ret_len, buf))
        } else {
            Ok(buf[..ret_len as usize].to_vec())
        }
    })
}

/// Returns data made available by the host under the given name. The type `D`
/// must be correctly specified, otherwise undefined behavior will occur.
pub fn meta_data<D>(name: &str) -> Option<D>
where
    D: Archive,
    D::Archived: Deserialize<D, Infallible>,
{
    let name_slice = name.as_bytes();

    let name = name_slice.as_ptr();
    let name_len = name_slice.len() as u32;

    unsafe {
        match ext::hd(name, name_len) as usize {
            0 => None,
            arg_pos => Some(with_arg_buf(|buf| {
                let ret = archived_root::<D>(&buf[..arg_pos]);
                ret.deserialize(&mut Infallible).expect("Infallible")
            })),
        }
    }
}

/// Return the current contract's owner.
pub fn owner<const N: usize>() -> [u8; N] {
    unsafe { ext::owner() };
    with_arg_buf(|buf| {
        let ret = unsafe { archived_root::<[u8; N]>(&buf[..N]) };
        ret.deserialize(&mut Infallible).expect("Infallible")
    })
}

/// Return the current contract's id.
pub fn self_id() -> ContractId {
    unsafe { ext::self_id() };
    let id: ContractId = with_arg_buf(|buf| {
        let ret =
            unsafe { archived_root::<ContractId>(&buf[..CONTRACT_ID_BYTES]) };
        ret.deserialize(&mut Infallible).expect("Infallible")
    });
    id
}

/// Return the ID of the calling contract. The returned id will be
/// uninitialized if there is no caller - meaning this is the first contract
/// to be called.
pub fn caller() -> ContractId {
    unsafe { ext::caller() };
    with_arg_buf(|buf| {
        let ret = unsafe {
            archived_root::<ContractId>(
                &buf[..core::mem::size_of::<Archived<ContractId>>()],
            )
        };
        ret.deserialize(&mut Infallible).expect("Infallible")
    })
}

/// Returns the points limit with which the contact was called.
pub fn limit() -> u64 {
    unsafe { ext::limit() }
}

/// Returns the amount of points the contact has spent.
pub fn spent() -> u64 {
    unsafe { ext::spent() }
}

/// Emits an event with the given data.
pub fn emit<D>(topic: &'static str, data: D)
where
    for<'a> D: Serialize<StandardBufSerializer<'a>>,
{
    with_arg_buf(|buf| {
        let mut sbuf = [0u8; SCRATCH_BUF_BYTES];
        let scratch = BufferScratch::new(&mut sbuf);
        let ser = BufferSerializer::new(buf);
        let mut composite =
            CompositeSerializer::new(ser, scratch, rkyv::Infallible);

        composite.serialize_value(&data).unwrap();
        let arg_len = composite.pos() as u32;

        let topic_ptr = topic.as_ptr();
        let topic_len = topic.len() as u32;

        unsafe { ext::emit(topic_ptr, topic_len, arg_len) }
    });
}

/// Feeds the host with data.
///
/// This is only allowed to be called in the context of a `feed_call`, and
/// will error out otherwise. It is meant for contracts to be able to report
/// large amounts of data to the host, in the span of a single call.
pub fn feed<D>(data: D)
where
    for<'a> D: Serialize<StandardBufSerializer<'a>>,
{
    with_arg_buf(|buf| {
        let mut sbuf = [0u8; SCRATCH_BUF_BYTES];
        let scratch = BufferScratch::new(&mut sbuf);
        let ser = BufferSerializer::new(buf);
        let mut composite =
            CompositeSerializer::new(ser, scratch, rkyv::Infallible);

        composite.serialize_value(&data).unwrap();
        let arg_len = composite.pos() as u32;

        unsafe { ext::feed(arg_len) }
    });
}
