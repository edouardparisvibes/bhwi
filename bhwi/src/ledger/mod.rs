mod command;
mod merkle;
mod store;

pub mod apdu;
pub mod error;
pub mod psbt;
pub mod wallet;

use bitcoin::{
    bip32::{DerivationPath, Fingerprint, Xpub},
    Network,
};
use std::str::FromStr;
pub use wallet::{WalletPolicy, WalletPubKey};

use crate::Interpreter;

use apdu::{ApduCommand, ApduError, ApduResponse, StatusWord};
use store::{DelegatedStore, StoreError};

#[derive(Debug)]
pub enum LedgerError {
    MissingCommandInfo(&'static str),
    NoErrorOrResult,
    Apdu(ApduError),
    Store(StoreError),
    Interrupted,
    UnexpectedResult(Vec<u8>),
    FailedToOpenApp(Vec<u8>),
}

impl From<ApduError> for LedgerError {
    fn from(value: ApduError) -> Self {
        LedgerError::Apdu(value)
    }
}

impl From<StoreError> for LedgerError {
    fn from(value: StoreError) -> Self {
        LedgerError::Store(value)
    }
}

#[derive(Clone, Debug)]
pub enum LedgerCommand {
    OpenApp(Network),
    GetMasterFingerprint,
    GetXpub { path: DerivationPath, display: bool },
}

pub enum LedgerResponse {
    TaskDone,
    MasterFingerprint(Fingerprint),
    Xpub(Xpub),
}

#[derive(Default)]
enum State {
    #[default]
    New,
    Running {
        command: LedgerCommand,
        store: Option<DelegatedStore>,
    },
    Finished(LedgerResponse),
}

pub struct LedgerInterpreter<C, T, R, E> {
    state: State,
    _marker: std::marker::PhantomData<(C, T, R, E)>,
}

impl<C, T, R, E> Default for LedgerInterpreter<C, T, R, E> {
    fn default() -> Self {
        Self {
            state: State::default(),
            _marker: std::marker::PhantomData,
        }
    }
}

impl<C, T, R, E> Interpreter for LedgerInterpreter<C, T, R, E>
where
    C: TryInto<LedgerCommand, Error = LedgerError>,
    T: From<ApduCommand>,
    R: From<LedgerResponse>,
    E: From<LedgerError>,
{
    type Command = C;
    type Transmit = T;
    type Response = R;
    type Error = E;

    fn start(&mut self, command: Self::Command) -> Result<Self::Transmit, Self::Error> {
        let command: LedgerCommand = command.try_into()?;
        let (transmit, store) = match command {
            LedgerCommand::GetMasterFingerprint => (
                Self::Transmit::from(command::get_master_fingerprint()),
                None,
            ),
            LedgerCommand::GetXpub { ref path, display } => (
                Self::Transmit::from(command::get_extended_pubkey(path, display)),
                None,
            ),
            LedgerCommand::OpenApp(network) => {
                (Self::Transmit::from(command::open_app(network)), None)
            }
        };
        self.state = State::Running { command, store };
        Ok(transmit)
    }
    fn exchange(&mut self, data: Vec<u8>) -> Result<Option<Self::Transmit>, Self::Error> {
        if let State::Running { store, command } = &mut self.state {
            let res = ApduResponse::try_from(data).map_err(LedgerError::from)?;
            if res.status_word == StatusWord::InterruptedExecution {
                if let Some(store) = store {
                    let transmit = store.execute(res.data).map_err(LedgerError::from)?;
                    return Ok(Some(Self::Transmit::from(command::continue_interrupted(
                        transmit,
                    ))));
                } else {
                    return Err(LedgerError::Interrupted.into());
                }
            }
            match command {
                LedgerCommand::GetMasterFingerprint => {
                    if res.data.len() < 4 {
                        return Err(LedgerError::UnexpectedResult(res.data).into());
                    } else {
                        let mut fg = [0x00; 4];
                        fg.copy_from_slice(&res.data[0..4]);
                        self.state = State::Finished(LedgerResponse::MasterFingerprint(
                            Fingerprint::from(fg),
                        ));
                    }
                }
                LedgerCommand::GetXpub { .. } => {
                    let xpub = Xpub::from_str(&String::from_utf8_lossy(&res.data))
                        .map_err(|_| LedgerError::UnexpectedResult(res.data))?;
                    self.state = State::Finished(LedgerResponse::Xpub(xpub));
                }
                LedgerCommand::OpenApp(..) => {
                    if res.status_word == StatusWord::OK ||
                    // An app is already open and the cla cannot be supported
                    res.status_word == StatusWord::ClaNotSupported
                    {
                        self.state = State::Finished(LedgerResponse::TaskDone);
                    } else {
                        return Err(LedgerError::UnexpectedResult(res.data).into());
                    }
                }
            }
        }
        Ok(None)
    }
    fn end(self) -> Result<Self::Response, Self::Error> {
        if let State::Finished(res) = self.state {
            Ok(Self::Response::from(res))
        } else {
            Err(LedgerError::NoErrorOrResult.into())
        }
    }
}
