// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.
//
// Copyright (c) DUSK NETWORK. All rights reserved.

use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::sync::Arc;

use colored::*;
use piecrust_uplink as uplink;
use piecrust_uplink::MODULE_ID_BYTES;
use uplink::ModuleId;
use wasmer::wasmparser::Operator;
use wasmer::{CompilerConfig, RuntimeError, Tunables, TypedFunction};
use wasmer_compiler_singlepass::Singlepass;
use wasmer_middlewares::metering::{
    get_remaining_points, set_remaining_points, MeteringPoints,
};
use wasmer_middlewares::Metering;
use wasmer_types::{
    MemoryError, MemoryStyle, MemoryType, TableStyle, TableType,
};
use wasmer_vm::{LinearMemory, VMMemory, VMTable, VMTableDefinition};

use crate::event::Event;
use crate::imports::DefaultImports;
use crate::module::WrappedModule;
use crate::session::Session;
use crate::store::Memory;
use crate::Error;
use crate::Error::InitalizationError;

pub struct WrappedInstance {
    instance: wasmer::Instance,
    arg_buf_ofs: usize,
    #[allow(unused)]
    heap_base: usize,
    store: wasmer::Store,
    meta_ofs: usize,
}

pub(crate) struct Env {
    self_id: ModuleId,
    session: &'static mut Session,
}

impl Deref for Env {
    type Target = Session;

    fn deref(&self) -> &Self::Target {
        self.session
    }
}

impl DerefMut for Env {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.session
    }
}

impl Env {
    pub fn self_instance<'b>(&self) -> &'b mut WrappedInstance {
        self.session
            .nth_from_top(0)
            .expect("there should be at least one element in the call stack")
            .instance
    }

    pub fn limit(&self) -> u64 {
        self.session
            .nth_from_top(0)
            .expect("there should be at least one element in the call stack")
            .limit
    }

    pub fn emit(&mut self, arg_len: u32) {
        let data = self.self_instance().with_arg_buffer(|buf| {
            let arg_len = arg_len as usize;
            Vec::from(&buf[..arg_len])
        });

        let event = Event::new(self.self_id, data);
        self.session.push_event(event);
    }
}

/// Convenience methods for dealing with our custom store
pub struct Store;

impl Store {
    const INITIAL_POINT_LIMIT: u64 = 10_000_000;

    pub fn new_store() -> wasmer::Store {
        Self::with_creator(|compiler_config| {
            wasmer::Store::new(compiler_config)
        })
    }

    pub fn new_store_with_tunables(
        tunables: impl Tunables + Send + Sync + 'static,
    ) -> wasmer::Store {
        Self::with_creator(|compiler_config| {
            wasmer::Store::new_with_tunables(compiler_config, tunables)
        })
    }

    fn with_creator<F>(f: F) -> wasmer::Store
    where
        F: FnOnce(Singlepass) -> wasmer::Store,
    {
        let metering =
            Arc::new(Metering::new(Self::INITIAL_POINT_LIMIT, cost_function));

        let mut compiler_config = Singlepass::default();
        compiler_config.push_middleware(metering);

        f(compiler_config)
    }
}

enum ModuleInitState {
    Unknown,
    Required,
    Done,
}
impl WrappedInstance {
    pub fn new(
        session: &mut Session,
        module_id: ModuleId,
        module: WrappedModule,
        memory: Memory,
    ) -> Result<Self, Error> {
        let tunables = InstanceTunables::new(memory.clone());
        let mut store = Store::new_store_with_tunables(tunables);

        let env = Env {
            self_id: module_id,
            /// # Safety
            /// Wasmer API requires that Env has a static lifetime.
            /// We can safely assume that Env won't outlive session
            /// as Env lifetime is limited by module's lifetime.
            /// Module's lifetime, in turn, won't exceed session's lifetime.
            /// Hence, we can safely enforce Env's lifetime to be static.
            session: unsafe {
                std::mem::transmute::<&mut Session, &'static mut Session>(
                    session,
                )
            },
        };

        let imports = DefaultImports::default(&mut store, env);

        let module =
            unsafe { wasmer::Module::deserialize(&store, module.as_bytes())? };

        let instance = wasmer::Instance::new(&mut store, &module, &imports)?;

        let arg_buf_ofs =
            match instance.exports.get_global("A")?.get(&mut store) {
                wasmer::Value::I32(i) => i as usize,
                _ => todo!("Missing `A` Argbuf export"),
            };

        let heap_base =
            match instance.exports.get_global("__heap_base")?.get(&mut store) {
                wasmer::Value::I32(i) => i as usize,
                _ => todo!("Missing heap base"),
            };

        let self_id_ofs =
            match instance.exports.get_global("SELF_ID")?.get(&mut store) {
                wasmer::Value::I32(i) => i as usize,
                _ => todo!("Missing `SELF_ID` export"),
            };

        let meta_ofs = match instance.exports.get_global("M")?.get(&mut store) {
            wasmer::Value::I32(i) => i as usize,
            _ => todo!("Missing `M` Metadata export"),
        };
        // write self id into memory.
        let mut memory_guard = memory.write();
        let bytes = memory_guard.as_bytes_mut();
        bytes[self_id_ofs..self_id_ofs + MODULE_ID_BYTES]
            .copy_from_slice(module_id.as_bytes());

        if bytes[meta_ofs] == ModuleInitState::Unknown as u8 {
            let init_function_exists =
                instance.exports.get_function("init").is_ok();
            bytes[meta_ofs] = if init_function_exists {
                ModuleInitState::Required as u8
            } else {
                ModuleInitState::Done as u8
            };
        }
        let wrapped = WrappedInstance {
            store,
            instance,
            arg_buf_ofs,
            heap_base,
            meta_ofs,
        };

        Ok(wrapped)
    }

    // Write argument into instance
    pub(crate) fn write_argument(&mut self, arg: &[u8]) {
        self.with_arg_buffer(|buf| buf[..arg.len()].copy_from_slice(arg))
    }

    // Read argument from instance
    pub(crate) fn read_argument(&mut self, arg: &mut [u8]) {
        self.with_arg_buffer(|buf| arg.copy_from_slice(&buf[..arg.len()]))
    }

    pub(crate) fn read_bytes_from_arg_buffer(&self, arg_len: u32) -> Vec<u8> {
        self.with_arg_buffer(|abuf| {
            let slice = &abuf[..arg_len as usize];
            slice.to_vec()
        })
    }

    pub(crate) fn with_memory<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        let mem =
            self.instance.exports.get_memory("memory").expect(
                "memory export should be checked at module creation time",
            );
        let view = mem.view(&self.store);
        let memory_bytes = unsafe { view.data_unchecked() };
        f(memory_bytes)
    }

    pub(crate) fn with_memory_mut<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mem =
            self.instance.exports.get_memory("memory").expect(
                "memory export should be checked at module creation time",
            );
        let view = mem.view(&self.store);
        let memory_bytes = unsafe { view.data_unchecked_mut() };
        f(memory_bytes)
    }

    pub(crate) fn with_arg_buffer<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        self.with_memory_mut(|memory_bytes| {
            let a = self.arg_buf_ofs;
            let b = uplink::ARGBUF_LEN;
            let begin = &mut memory_bytes[a..];
            let trimmed = &mut begin[..b];
            f(trimmed)
        })
    }

    pub(crate) fn with_meta_buffer<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        self.with_memory_mut(|memory_bytes| {
            let a = self.meta_ofs;
            let b = uplink::METADATA_LEN;
            let begin = &mut memory_bytes[a..];
            let trimmed = &mut begin[..b];
            f(trimmed)
        })
    }
    pub(crate) fn write_bytes_to_arg_buffer(&self, buf: &[u8]) -> u32 {
        self.with_arg_buffer(|arg_buffer| {
            arg_buffer[..buf.len()].copy_from_slice(buf);
            buf.len() as u32
        })
    }

    pub fn query(
        &mut self,
        method_name: &str,
        arg_len: u32,
        limit: u64,
    ) -> Result<i32, Error> {
        let fun: TypedFunction<u32, i32> = self
            .instance
            .exports
            .get_typed_function(&self.store, method_name)?;

        self.set_remaining_points(limit);
        fun.call(&mut self.store, arg_len)
            .map_err(|e| map_call_err(self, e))
    }

    pub fn transact(
        &mut self,
        method_name: &str,
        arg_len: u32,
        limit: u64,
    ) -> Result<i32, Error> {
        if !self.is_initialized() {
            return Err(InitalizationError);
        }
        let fun: TypedFunction<u32, i32> = self
            .instance
            .exports
            .get_typed_function(&self.store, method_name)?;

        self.set_remaining_points(limit);
        fun.call(&mut self.store, arg_len)
            .map_err(|e| map_call_err(self, e))
    }

    pub fn set_remaining_points(&mut self, limit: u64) {
        set_remaining_points(&mut self.store, &self.instance, limit);
    }

    pub fn get_remaining_points(&mut self) -> Option<u64> {
        match get_remaining_points(&mut self.store, &self.instance) {
            MeteringPoints::Remaining(points) => Some(points),
            MeteringPoints::Exhausted => None,
        }
    }

    #[allow(unused)]
    pub fn snap(&self) {
        let mem = self
            .instance
            .exports
            .get_memory("memory")
            .expect("memory export is checked at module creation time");

        let view = mem.view(&self.store);
        let maybe_interesting = unsafe { view.data_unchecked_mut() };

        const CSZ: usize = 128;
        const RSZ: usize = 16;

        for (chunk_nr, chunk) in maybe_interesting.chunks(CSZ).enumerate() {
            if chunk[..] != [0; CSZ][..] {
                for (row_nr, row) in chunk.chunks(RSZ).enumerate() {
                    let ofs = chunk_nr * CSZ + row_nr * RSZ;

                    print!("{ofs:08x}:");

                    for (i, byte) in row.iter().enumerate() {
                        if i % 4 == 0 {
                            print!(" ");
                        }

                        let buf_start = self.arg_buf_ofs;
                        let buf_end = buf_start + uplink::ARGBUF_LEN;
                        let heap_base = self.heap_base;

                        if ofs + i >= buf_start && ofs + i < buf_end {
                            print!("{}", format!("{byte:02x}").red());
                            print!(" ");
                        } else if ofs + i >= heap_base {
                            print!("{}", format!("{byte:02x} ").green());
                        } else {
                            print!("{byte:02x} ")
                        }
                    }

                    println!();
                }
            }
        }
    }

    pub fn arg_buffer_offset(&self) -> usize {
        self.arg_buf_ofs
    }
    pub fn is_initialized(&self) -> bool {
        self.with_meta_buffer(|meta_buf| {
            meta_buf[0] == ModuleInitState::Done as u8
        })
    }
    pub fn set_initialized(&mut self) {
        self.with_meta_buffer(|meta_buf| {
            meta_buf[0] = ModuleInitState::Done as u8;
        });
    }
}

fn map_call_err(instance: &mut WrappedInstance, err: RuntimeError) -> Error {
    if instance.get_remaining_points().is_none() {
        return Error::OutOfPoints;
    }

    err.into()
}

pub struct InstanceTunables {
    memory: Memory,
}

impl InstanceTunables {
    pub fn new(memory: Memory) -> Self {
        InstanceTunables { memory }
    }
}

impl Tunables for InstanceTunables {
    fn memory_style(&self, _memory: &MemoryType) -> MemoryStyle {
        self.memory.style()
    }

    fn table_style(&self, _table: &TableType) -> TableStyle {
        TableStyle::CallerChecksSignature
    }

    fn create_host_memory(
        &self,
        _ty: &MemoryType,
        _style: &MemoryStyle,
    ) -> Result<VMMemory, MemoryError> {
        Ok(VMMemory::from_custom(self.memory.clone()))
    }

    unsafe fn create_vm_memory(
        &self,
        _ty: &MemoryType,
        _style: &MemoryStyle,
        vm_definition_location: NonNull<wasmer_vm::VMMemoryDefinition>,
    ) -> Result<VMMemory, MemoryError> {
        // now, it's important to update vm_definition_location with the memory
        // information!
        let mut ptr = vm_definition_location;
        let md = ptr.as_mut();

        let memory = self.memory.clone();

        *md = *memory.vmmemory().as_ptr();

        Ok(memory.into())
    }

    /// Create a table owned by the host given a [`TableType`] and a
    /// [`TableStyle`].
    fn create_host_table(
        &self,
        ty: &TableType,
        style: &TableStyle,
    ) -> Result<VMTable, String> {
        VMTable::new(ty, style)
    }

    unsafe fn create_vm_table(
        &self,
        ty: &TableType,
        style: &TableStyle,
        vm_definition_location: NonNull<VMTableDefinition>,
    ) -> Result<VMTable, String> {
        VMTable::from_definition(ty, style, vm_definition_location)
    }
}

fn cost_function(_op: &Operator) -> u64 {
    1
}
