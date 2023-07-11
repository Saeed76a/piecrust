// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.
//
// Copyright (c) DUSK NETWORK. All rights reserved.

use std::borrow::Cow;
use std::sync::Arc;
use thiserror::Error;

use piecrust_uplink::{ContractError, ContractId};
use rkyv::ser::serializers::{
    BufferSerializerError, CompositeSerializerError, FixedSizeScratchError,
};

pub type Compo = CompositeSerializerError<
    BufferSerializerError,
    FixedSizeScratchError,
    std::convert::Infallible,
>;

/// The error type returned by the piecrust VM.
#[derive(Error, Debug, Clone)]
pub enum Error {
    #[error("Commit error: {0}")]
    CommitError(Cow<'static, str>),
    #[error(transparent)]
    CompileError(Arc<wasmer::CompileError>),
    #[error(transparent)]
    CompositeSerializerError(Arc<Compo>),
    #[error(transparent)]
    ContractCacheError(Arc<std::io::Error>),
    #[error("Contract does not exist: {0}")]
    ContractDoesNotExist(ContractId),
    #[error(transparent)]
    DeserializeError(Arc<wasmer::DeserializeError>),
    #[error(transparent)]
    ExportError(Arc<wasmer::ExportError>),
    #[error("InitalizationError: {0}")]
    InitalizationError(Cow<'static, str>),
    #[error(transparent)]
    InstantiationError(Arc<wasmer::InstantiationError>),
    #[error(transparent)]
    MemorySetupError(Arc<std::io::Error>),
    #[error("OutOfPoints")]
    OutOfPoints,
    #[error(transparent)]
    ParsingError(wasmer::wasmparser::BinaryReaderError),
    #[error(transparent)]
    PersistenceError(Arc<std::io::Error>),
    #[error(transparent)]
    RestoreError(Arc<std::io::Error>),
    #[error(transparent)]
    RuntimeError(wasmer::RuntimeError),
    #[error(transparent)]
    SerializeError(Arc<wasmer::SerializeError>),
    #[error("Session error: {0}")]
    SessionError(Cow<'static, str>),
    #[error("WASMER TRAP")]
    Trap(Arc<wasmer_vm::Trap>),
    #[error(transparent)]
    Utf8(std::str::Utf8Error),
    #[error("ValidationError")]
    ValidationError,
}

impl From<std::str::Utf8Error> for Error {
    fn from(err: std::str::Utf8Error) -> Self {
        Self::Utf8(err)
    }
}

impl From<wasmer::InstantiationError> for Error {
    fn from(e: wasmer::InstantiationError) -> Self {
        Error::InstantiationError(Arc::from(e))
    }
}

impl From<wasmer::CompileError> for Error {
    fn from(e: wasmer::CompileError) -> Self {
        Error::CompileError(Arc::from(e))
    }
}

impl From<wasmer::ExportError> for Error {
    fn from(e: wasmer::ExportError) -> Self {
        Error::ExportError(Arc::from(e))
    }
}

impl From<wasmer::RuntimeError> for Error {
    fn from(e: wasmer::RuntimeError) -> Self {
        Error::RuntimeError(e)
    }
}

impl From<wasmer::SerializeError> for Error {
    fn from(e: wasmer::SerializeError) -> Self {
        Error::SerializeError(Arc::from(e))
    }
}

impl From<wasmer::DeserializeError> for Error {
    fn from(e: wasmer::DeserializeError) -> Self {
        Error::DeserializeError(Arc::from(e))
    }
}

impl From<Compo> for Error {
    fn from(e: Compo) -> Self {
        Error::CompositeSerializerError(Arc::from(e))
    }
}

impl From<wasmer_vm::Trap> for Error {
    fn from(e: wasmer_vm::Trap) -> Self {
        Error::Trap(Arc::from(e))
    }
}

impl<A, B> From<rkyv::validation::CheckArchiveError<A, B>> for Error {
    fn from(_e: rkyv::validation::CheckArchiveError<A, B>) -> Self {
        Error::ValidationError
    }
}

const OTHER_STATUS_CODE: i32 = i32::MIN;

impl From<Error> for ContractError {
    fn from(err: Error) -> Self {
        // TODO implement this fully
        match err {
            Error::OutOfPoints => Self::OUTOFGAS,
            _ => Self::OTHER(OTHER_STATUS_CODE),
        }
    }
}
