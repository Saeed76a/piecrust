// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.
//
// Copyright (c) DUSK NETWORK. All rights reserved.

pub mod call_stack;

use std::borrow::Cow;
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::mem;
use std::sync::Arc;

use bytecheck::CheckBytes;
use piecrust_uplink::{ModuleId, SCRATCH_BUF_BYTES};
use rkyv::ser::serializers::{
    BufferScratch, BufferSerializer, CompositeSerializer,
};
use rkyv::ser::Serializer;
use rkyv::{
    check_archived_root, validation::validators::DefaultValidator, Archive,
    Deserialize, Infallible, Serialize,
};
use wasmer_types::WASM_PAGE_SIZE;

use crate::event::Event;
use crate::instance::WrappedInstance;
use crate::module::WrappedModule;
use crate::store::ModuleSession;
use crate::types::StandardBufSerializer;
use crate::vm::HostQueries;
use crate::Error;
use crate::Error::PersistenceError;

use call_stack::{CallStack, StackElementView};

const DEFAULT_LIMIT: u64 = 65_536;
const MAX_META_SIZE: usize = 65_536;

unsafe impl Send for Session {}
unsafe impl Sync for Session {}

pub struct Session {
    call_stack: CallStack,
    debug: Vec<String>,
    events: Vec<Event>,
    data: Metadata,

    module_session: ModuleSession,
    host_queries: HostQueries,

    limit: u64,
    spent: u64,

    call_history: Vec<CallOrDeploy>,
    buffer: Vec<u8>,

    call_count: usize,
    icc_count: usize, // inter-contract call - 0 is the main call
    icc_height: usize, // height of an inter-contract call
    // Keeps errors/successes that were found during the execution of a
    // particular inter-contract call in the context of a call.
    icc_errors: BTreeMap<usize, BTreeMap<usize, Error>>,
}

impl Session {
    pub(crate) fn new(
        module_session: ModuleSession,
        host_queries: HostQueries,
    ) -> Self {
        Session {
            call_stack: CallStack::new(),
            debug: vec![],
            events: vec![],
            data: Metadata::new(),
            module_session,
            host_queries,
            limit: DEFAULT_LIMIT,
            spent: 0,
            call_history: vec![],
            buffer: vec![0; WASM_PAGE_SIZE],
            call_count: 0,
            icc_count: 0,
            icc_height: 0,
            icc_errors: BTreeMap::new(),
        }
    }

    /// Deploy a module, returning its `ModuleId`. The ID is computed using a
    /// `blake3` hash of the bytecode.
    ///
    /// If one needs to specify the ID, [`deploy_with_id`] is available.
    ///
    /// [`deploy_with_id`]: `Session::deploy_with_id`
    pub fn deploy(&mut self, bytecode: &[u8]) -> Result<ModuleId, Error> {
        let module_id = self
            .module_session
            .deploy(bytecode)
            .map_err(|err| PersistenceError(Arc::new(err)))?;

        self.call_history.push(From::from(Deploy {
            module_id,
            bytecode: bytecode.to_vec(),
        }));

        Ok(module_id)
    }

    /// Deploy a module with the given ID.
    ///
    /// If one would like to *not* specify the `ModuleId`, [`deploy`] is
    /// available.
    ///
    /// [`deploy`]: `Session::deploy`
    pub fn deploy_with_id(
        &mut self,
        module_id: ModuleId,
        bytecode: &[u8],
    ) -> Result<(), Error> {
        self.module_session
            .deploy_with_id(module_id, bytecode)
            .map_err(|err| PersistenceError(Arc::new(err)))?;

        self.call_history.push(From::from(Deploy {
            module_id,
            bytecode: bytecode.to_vec(),
        }));

        Ok(())
    }

    pub fn query<Arg, Ret>(
        &mut self,
        module: ModuleId,
        method_name: &str,
        arg: &Arg,
    ) -> Result<Ret, Error>
    where
        Arg: for<'b> Serialize<StandardBufSerializer<'b>>,
        Ret: Archive,
        Ret::Archived: Deserialize<Ret, Infallible>
            + for<'b> CheckBytes<DefaultValidator<'b>>,
    {
        let mut sbuf = [0u8; SCRATCH_BUF_BYTES];
        let scratch = BufferScratch::new(&mut sbuf);
        let ser = BufferSerializer::new(&mut self.buffer[..]);
        let mut ser = CompositeSerializer::new(ser, scratch, Infallible);

        ser.serialize_value(arg).expect("Infallible");
        let pos = ser.pos();

        let ret_bytes = self.re_execute_until_ok(Call {
            ty: CallType::Q,
            module,
            fname: method_name.to_string(),
            fdata: self.buffer[..pos].to_vec(),
            limit: self.limit,
        })?;

        let ta = check_archived_root::<Ret>(&ret_bytes[..])?;
        let ret = ta.deserialize(&mut Infallible).expect("Infallible");

        Ok(ret)
    }

    pub fn transact<Arg, Ret>(
        &mut self,
        module: ModuleId,
        method_name: &str,
        arg: &Arg,
    ) -> Result<Ret, Error>
    where
        Arg: for<'b> Serialize<StandardBufSerializer<'b>>,
        Ret: Archive,
        Ret::Archived: Deserialize<Ret, Infallible>
            + for<'b> CheckBytes<DefaultValidator<'b>>,
    {
        let mut sbuf = [0u8; SCRATCH_BUF_BYTES];
        let scratch = BufferScratch::new(&mut sbuf);
        let ser = BufferSerializer::new(&mut self.buffer[..]);
        let mut ser = CompositeSerializer::new(ser, scratch, Infallible);

        ser.serialize_value(arg).expect("Infallible");
        let pos = ser.pos();

        let ret_bytes = self.re_execute_until_ok(Call {
            ty: CallType::T,
            module,
            fname: method_name.to_string(),
            fdata: self.buffer[..pos].to_vec(),
            limit: self.limit,
        })?;

        let ta = check_archived_root::<Ret>(&ret_bytes[..])?;
        let ret = ta.deserialize(&mut Infallible).expect("Infallible");

        Ok(ret)
    }

    pub fn root(&self) -> [u8; 32] {
        self.module_session.root()
    }

    pub(crate) fn push_event(&mut self, event: Event) {
        self.events.push(event);
    }

    fn new_instance(
        &mut self,
        module_id: ModuleId,
    ) -> Result<WrappedInstance, Error> {
        let (bytecode, memory) = self
            .module_session
            .module(module_id)
            .map_err(|err| PersistenceError(Arc::new(err)))?
            .expect("Module should exist");

        let module = WrappedModule::new(&bytecode)?;
        let instance = WrappedInstance::new(self, module_id, module, memory)?;

        Ok(instance)
    }

    pub(crate) fn host_query(
        &self,
        name: &str,
        buf: &mut [u8],
        arg_len: u32,
    ) -> Option<u32> {
        self.host_queries.call(name, buf, arg_len)
    }

    /// Sets the point limit for the next call to `query` or `transact`.
    pub fn set_point_limit(&mut self, limit: u64) {
        self.limit = limit
    }

    pub fn spent(&self) -> u64 {
        self.spent
    }

    pub(crate) fn nth_from_top<'a>(
        &self,
        n: usize,
    ) -> Option<StackElementView<'a>> {
        self.call_stack.nth_from_top(n)
    }

    pub(crate) fn push_callstack<'b>(
        &mut self,
        module_id: ModuleId,
        limit: u64,
    ) -> Result<StackElementView<'b>, Error> {
        let instance = self.call_stack.instance(&module_id);

        match instance {
            Some(_) => {
                self.call_stack.push(module_id, limit);
            }
            None => {
                let instance = self.new_instance(module_id)?;
                self.call_stack.push_instance(module_id, limit, instance);
            }
        }

        Ok(self
            .call_stack
            .nth_from_top(0)
            .expect("We just pushed an element to the stack"))
    }

    pub(crate) fn pop_callstack(&mut self) {
        self.call_stack.pop();
    }

    pub fn commit(self) -> Result<[u8; 32], Error> {
        self.module_session
            .commit()
            .map_err(|err| PersistenceError(Arc::new(err)))
    }

    pub(crate) fn register_debug<M: Into<String>>(&mut self, msg: M) {
        self.debug.push(msg.into());
    }

    pub fn take_events(&mut self) -> Vec<Event> {
        mem::take(&mut self.events)
    }

    pub fn with_debug<C, R>(&self, c: C) -> R
    where
        C: FnOnce(&[String]) -> R,
    {
        c(&self.debug)
    }

    pub fn meta(&self, name: &str) -> Option<Vec<u8>> {
        self.data.get(name)
    }

    pub fn set_meta<S, V>(&mut self, name: S, value: V)
    where
        S: Into<Cow<'static, str>>,
        V: for<'a> Serialize<StandardBufSerializer<'a>>,
    {
        let mut buf = [0u8; MAX_META_SIZE];
        let mut sbuf = [0u8; SCRATCH_BUF_BYTES];

        let ser = BufferSerializer::new(&mut buf[..]);
        let scratch = BufferScratch::new(&mut sbuf);

        let mut serializer =
            StandardBufSerializer::new(ser, scratch, Infallible);
        serializer.serialize_value(&value).expect("Infallible");

        let pos = serializer.pos();

        let data = buf[..pos].to_vec();
        self.data.insert(name, data);
    }

    /// Increment the call execution count.
    ///
    /// If the call errors on the first called module, return said error.
    pub(crate) fn increment_call_count(&mut self) -> Option<Error> {
        self.call_count += 1;
        self.icc_errors
            .get(&self.call_count)
            .and_then(|map| map.get(&0))
            .cloned()
    }

    /// Increment the icc execution count, returning the current count. If there
    /// was, previously, an error in the execution of the ic call with the
    /// current number count - meaning after iteration - it will be returned.
    pub(crate) fn increment_icc_count(&mut self) -> Option<Error> {
        self.icc_count += 1;
        match self.icc_errors.get(&self.call_count) {
            Some(icc_results) => icc_results.get(&self.icc_count).cloned(),
            None => None,
        }
    }

    /// When this is decremented, it means we have successfully "rolled back"
    /// one icc. Therefore it should remove all errors after the call, after the
    /// decrement.
    ///
    /// # Panics
    /// When the errors map is not present.
    pub(crate) fn decrement_icc_count(&mut self) {
        self.icc_count -= 1;
        self.icc_errors
            .get_mut(&self.call_count)
            .expect("Map should always be there")
            .retain(|c, _| c <= &self.icc_count);
    }

    /// Increments the height of an icc.
    pub(crate) fn increment_icc_height(&mut self) {
        self.icc_height += 1;
    }

    /// Decrements the height of an icc.
    pub(crate) fn decrement_icc_height(&mut self) {
        self.icc_height -= 1;
    }

    /// Insert error at the current icc count.
    ///
    /// If there are errors at a larger ICC count than current, they will be
    /// forgotten.
    pub(crate) fn insert_icc_error(&mut self, err: Error) {
        match self.icc_errors.entry(self.call_count) {
            Entry::Vacant(entry) => {
                let mut map = BTreeMap::new();
                map.insert(self.icc_count, err);
                entry.insert(map);
            }
            Entry::Occupied(mut entry) => {
                let map = entry.get_mut();
                map.insert(self.icc_count, err);
            }
        }
    }

    /// Execute the call and re-execute until the call errors with only itself
    /// in the call stack.
    fn re_execute_until_ok(&mut self, call: Call) -> Result<Vec<u8>, Error> {
        // If the call succeeds at first run, then we can proceed with adding it
        // to the call history and return.
        match self.call_if_not_error(call) {
            Ok(data) => return Ok(data),
            Err(err) => {
                // If the call does not succeed, we should check if it failed at
                // height zero. If so, we should register the error with ICC
                // count 0 and re-execute, returning the result.
                //
                // This will ensure that the call is never really executed,
                // keeping it atomic.
                if self.icc_height == 0 {
                    self.icc_count = 0;
                    self.insert_icc_error(err);
                    return self.re_execute();
                }

                // If it is not at height zero, just register the error and let
                // it re-execute until ok.
                self.insert_icc_error(err);
            }
        }

        // Loop until executed atomically.
        loop {
            match self.re_execute() {
                Ok(awesome) => return Ok(awesome),
                Err(err) => {
                    if self.icc_height == 0 {
                        self.icc_count = 0;
                        self.insert_icc_error(err);
                        return self.re_execute();
                    }
                    self.insert_icc_error(err);
                }
            }
        }
    }

    /// Purge all produced data and re-execute all transactions and deployments
    /// in order, returning the result of the last executed call.
    fn re_execute(&mut self) -> Result<Vec<u8>, Error> {
        println!("RE-EXECUTION");

        // Take all transaction history since we're going to re-add it back
        // anyway.
        let mut call_history = Vec::with_capacity(self.call_history.len());
        mem::swap(&mut call_history, &mut self.call_history);

        // Purge all other data that is set by performing transactions.
        self.call_stack.clear();
        self.debug.clear();
        self.events.clear();
        self.module_session.clear_modules();
        self.call_count = 0;

        // TODO Figure out how to handle metadata and point limit.
        //      It is important to preserve their value per call.
        //      Right now it probably won't bite us, since we're using it
        //      "properly", and not setting these pieces of data during the
        //      session, but only at the beginning.

        // This will always be set by the loop, so this one will never be
        // returned.
        let mut res = Ok(vec![]);

        for call in call_history {
            match call {
                CallOrDeploy::Call(call) => {
                    res = self.call_if_not_error(call);
                }
                CallOrDeploy::Deploy(deploy) => {
                    self.deploy_with_id(deploy.module_id, &deploy.bytecode)
                        .expect("Only deploys that succeed should be added to the history");
                }
            }
        }

        res
    }

    /// Make the call only if an error is not known. If an error is known return
    /// it instead.
    ///
    /// This will add the call to the call history as well.
    fn call_if_not_error(&mut self, call: Call) -> Result<Vec<u8>, Error> {
        // Set both the count and height of the ICCs to zero
        self.icc_count = 0;
        self.icc_height = 0;

        // If we already know of an error on this call, don't execute and just
        // return the error.
        if let Some(err) = self.increment_call_count() {
            // We also need it in the call history here.
            self.call_history.push(call.into());
            return Err(err);
        }

        let res = self.call_inner(&call);
        self.call_history.push(call.into());
        res
    }

    fn call_inner(&mut self, call: &Call) -> Result<Vec<u8>, Error> {
        let instance = self.push_callstack(call.module, call.limit)?.instance;

        let arg_len = instance.write_bytes_to_arg_buffer(&call.fdata);
        let ret_len = match call.ty {
            CallType::Q => instance.query(&call.fname, arg_len, call.limit),
            CallType::T => instance.transact(&call.fname, arg_len, call.limit),
        }?;
        let ret = instance.read_bytes_from_arg_buffer(ret_len as u32);

        self.spent = call.limit
            - instance
                .get_remaining_points()
                .expect("there should be remaining points");

        self.pop_callstack();

        Ok(ret)
    }
}

#[derive(Debug)]
enum CallOrDeploy {
    Call(Call),
    Deploy(Deploy),
}

impl From<Call> for CallOrDeploy {
    fn from(call: Call) -> Self {
        Self::Call(call)
    }
}

impl From<Deploy> for CallOrDeploy {
    fn from(deploy: Deploy) -> Self {
        Self::Deploy(deploy)
    }
}

#[derive(Debug)]
struct Deploy {
    module_id: ModuleId,
    bytecode: Vec<u8>,
}

#[derive(Debug)]
enum CallType {
    Q,
    T,
}

#[derive(Debug)]
struct Call {
    ty: CallType,
    module: ModuleId,
    fname: String,
    fdata: Vec<u8>,
    limit: u64,
}

#[derive(Debug)]
pub struct Metadata {
    data: BTreeMap<Cow<'static, str>, Vec<u8>>,
}

impl Metadata {
    fn new() -> Self {
        Self {
            data: BTreeMap::new(),
        }
    }

    fn insert<S>(&mut self, name: S, data: Vec<u8>)
    where
        S: Into<Cow<'static, str>>,
    {
        self.data.insert(name.into(), data);
    }

    fn get(&self, name: &str) -> Option<Vec<u8>> {
        self.data.get(name).cloned()
    }
}
