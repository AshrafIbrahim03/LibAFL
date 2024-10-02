//! Centralized event manager is a special event manager that will be used to achieve a more efficient message passing architecture.

// Some technical details..
// A very standard multi-process fuzzing using centralized event manager will consist of 4 components
// 1. The "fuzzer clients", the fuzzer that will do the "normal" fuzzing
// 2. The "centralized broker, the broker that gathers all the testcases from all the fuzzer clients
// 3. The "main evaluator", the evaluator node that will evaluate all the testcases pass by the centralized event manager to see if the testcases are worth propagating
// 4. The "main broker", the gathers the stats from the fuzzer clients and broadcast the newly found testcases from the main evaluator.

use alloc::{string::String, vec::Vec};
use core::{fmt::Debug, time::Duration};
use std::process;

#[cfg(feature = "llmp_compression")]
use libafl_bolts::{
    compress::GzipCompressor,
    llmp::{LLMP_FLAG_COMPRESSED, LLMP_FLAG_INITIALIZED},
};
use libafl_bolts::{
    llmp::{LlmpClient, LlmpClientDescription, Tag},
    shmem::{NopShMemProvider, ShMemProvider},
    tuples::{Handle, MatchNameRef},
    ClientId,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use super::{
    default_maybe_report_progress, default_report_progress, CanSerializeObserver, ManagerExit,
    NopEventManager,
};
#[cfg(feature = "llmp_compression")]
use crate::events::llmp::COMPRESS_THRESHOLD;
#[cfg(feature = "scalability_introspection")]
use crate::state::HasScalabilityMonitor;
use crate::{
    corpus::Corpus,
    events::{
        serialize_observers_adaptive, AdaptiveSerializer, Event, EventConfig, EventFirer,
        EventManagerHooksTuple, EventManagerId, EventProcessor, EventRestarter, HasEventManagerId,
        LogSeverity, ProgressReporter,
    },
    executors::{Executor, HasObservers},
    fuzzer::{EvaluatorObservers, ExecutionProcessor},
    inputs::{Input, UsesInput},
    observers::{ObserversTuple, TimeObserver},
    state::{HasCorpus, HasExecutions, HasLastReportTime, State, Stoppable, UsesState},
    Error, HasMetadata,
};

pub(crate) const _LLMP_TAG_TO_MAIN: Tag = Tag(0x3453453);

/// A wrapper manager to implement a main-secondary architecture with another broker
#[derive(Debug)]
pub struct CentralizedEventManager<EM, EMH, SP>
where
    SP: ShMemProvider,
{
    inner: EM,
    /// The centralized LLMP client for inter process communication
    client: LlmpClient<SP>,
    #[cfg(feature = "llmp_compression")]
    compressor: GzipCompressor,
    hooks: EMH,
    is_main: bool,
}

impl CentralizedEventManager<NopEventManager, (), NopShMemProvider> {
    /// Creates a builder for [`CentralizedEventManager`]
    #[must_use]
    pub fn builder() -> CentralizedEventManagerBuilder {
        CentralizedEventManagerBuilder::new()
    }
}

/// The builder or `CentralizedEventManager`
#[derive(Debug)]
pub struct CentralizedEventManagerBuilder {
    is_main: bool,
}

impl Default for CentralizedEventManagerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl CentralizedEventManagerBuilder {
    /// The constructor
    #[must_use]
    pub fn new() -> Self {
        Self { is_main: false }
    }

    /// Make this a main evaluator node
    #[must_use]
    pub fn is_main(self, is_main: bool) -> Self {
        Self { is_main }
    }

    /// Creates a new [`CentralizedEventManager`].
    pub fn build_from_client<EM, EMH, SP>(
        self,
        inner: EM,
        hooks: EMH,
        client: LlmpClient<SP>,
        time_obs: Option<Handle<TimeObserver>>,
    ) -> Result<CentralizedEventManager<EM, EMH, SP>, Error>
    where
        SP: ShMemProvider,
    {
        Ok(CentralizedEventManager {
            inner,
            hooks,
            client,
            #[cfg(feature = "llmp_compression")]
            compressor: GzipCompressor::with_threshold(COMPRESS_THRESHOLD),
            is_main: self.is_main,
        })
    }

    /// Create a centralized event manager on a port
    ///
    /// If the port is not yet bound, it will act as a broker; otherwise, it
    /// will act as a client.
    #[cfg(feature = "std")]
    pub fn build_on_port<EM, EMH, SP>(
        self,
        inner: EM,
        hooks: EMH,
        shmem_provider: SP,
        port: u16,
        time_obs: Option<Handle<TimeObserver>>,
    ) -> Result<CentralizedEventManager<EM, EMH, SP>, Error>
    where
        SP: ShMemProvider,
    {
        let client = LlmpClient::create_attach_to_tcp(shmem_provider, port)?;
        Ok(CentralizedEventManager {
            inner,
            hooks,
            client,
            #[cfg(feature = "llmp_compression")]
            compressor: GzipCompressor::with_threshold(COMPRESS_THRESHOLD),
            is_main: self.is_main,
        })
    }

    /// If a client respawns, it may reuse the existing connection, previously
    /// stored by [`LlmpClient::to_env()`].
    #[cfg(feature = "std")]
    pub fn build_existing_client_from_env<EM, EMH, SP>(
        self,
        inner: EM,
        hooks: EMH,
        shmem_provider: SP,
        env_name: &str,
        time_obs: Option<Handle<TimeObserver>>,
    ) -> Result<CentralizedEventManager<EM, EMH, SP>, Error>
    where
        SP: ShMemProvider,
    {
        Ok(CentralizedEventManager {
            inner,
            hooks,
            client: LlmpClient::on_existing_from_env(shmem_provider, env_name)?,
            #[cfg(feature = "llmp_compression")]
            compressor: GzipCompressor::with_threshold(COMPRESS_THRESHOLD),
            is_main: self.is_main,
        })
    }

    /// Create an existing client from description
    #[cfg(feature = "std")]
    pub fn existing_client_from_description<EM, EMH, SP>(
        self,
        inner: EM,
        hooks: EMH,
        shmem_provider: SP,
        description: &LlmpClientDescription,
        time_obs: Option<Handle<TimeObserver>>,
    ) -> Result<CentralizedEventManager<EM, EMH, SP>, Error>
    where
        SP: ShMemProvider,
    {
        Ok(CentralizedEventManager {
            inner,
            hooks,
            client: LlmpClient::existing_client_from_description(shmem_provider, description)?,
            #[cfg(feature = "llmp_compression")]
            compressor: GzipCompressor::with_threshold(COMPRESS_THRESHOLD),
            is_main: self.is_main,
        })
    }
}

impl<EM, EMH, S, SP> EventFirer<<S::Corpus as Corpus>::Input, S>
    for CentralizedEventManager<EM, EMH, SP>
where
    S: HasCorpus,
    SP: ShMemProvider,
    EM: HasEventManagerId + EventFirer<<S::Corpus as Corpus>::Input, S>,
    <S::Corpus as Corpus>::Input:,
{
    fn should_send(&self) -> bool {
        self.inner.should_send()
    }

    #[allow(clippy::match_same_arms)]
    fn fire(
        &mut self,
        state: &mut S,
        mut event: Event<<S::Corpus as Corpus>::Input>,
    ) -> Result<(), Error> {
        if !self.is_main {
            // secondary node
            let mut is_tc = false;
            // Forward to main only if new tc or heartbeat
            let should_be_forwarded = match &mut event {
                Event::NewTestcase { forward_id, .. } => {
                    *forward_id = Some(ClientId(self.inner.mgr_id().0 as u32));
                    is_tc = true;
                    true
                }
                Event::UpdateExecStats { .. } => true, // send it but this guy won't be handled. the only purpose is to keep this client alive else the broker thinks it is dead and will dc it
                Event::Stop => true,
                _ => false,
            };

            if should_be_forwarded {
                self.forward_to_main(&event)?;
                if is_tc {
                    // early return here because we only send it to centralized not main broker.
                    return Ok(());
                }
            }
        }

        // now inner llmp manager will process it if it's not a new testcase from a secondary node.
        self.inner.fire(state, event)
    }

    fn log(
        &mut self,
        state: &mut S,
        severity_level: LogSeverity,
        message: String,
    ) -> Result<(), Error> {
        self.inner.log(state, severity_level, message)
    }
    fn configuration(&self) -> EventConfig {
        self.inner.configuration()
    }
}

impl<EM, EMH, S, SP> EventRestarter<S> for CentralizedEventManager<EM, EMH, SP>
where
    EM: EventRestarter<S>,
    SP: ShMemProvider,
{
    #[inline]
    fn on_restart(&mut self, state: &mut S) -> Result<(), Error> {
        self.client.await_safe_to_unmap_blocking();
        self.inner.on_restart(state)?;
        Ok(())
    }
}

impl<EM, EMH, SP> ManagerExit for CentralizedEventManager<EM, EMH, SP>
where
    EM: ManagerExit,
    SP: ShMemProvider,
{
    fn send_exiting(&mut self) -> Result<(), Error> {
        self.client.sender_mut().send_exiting()?;
        self.inner.send_exiting()
    }

    #[inline]
    fn await_restart_safe(&mut self) {
        self.client.await_safe_to_unmap_blocking();
        self.inner.await_restart_safe();
    }
}

impl<E, EM, EMH, S, SP, Z> EventProcessor<E, S, Z> for CentralizedEventManager<EM, EMH, SP>
where
    E: HasObservers,
    E::Observers: DeserializeOwned,
    EM: EventProcessor<E, S, Z> + HasEventManagerId + EventFirer<<S::Corpus as Corpus>::Input, S>,
    EMH: EventManagerHooksTuple<<S::Corpus as Corpus>::Input, S>,
    SP: ShMemProvider,
    S: HasCorpus + Stoppable,
    <S::Corpus as Corpus>::Input: Input,
{
    fn process(&mut self, fuzzer: &mut Z, state: &mut S, executor: &mut E) -> Result<usize, Error> {
        if self.is_main {
            // main node
            self.receive_from_secondary(fuzzer, state, executor)
            // self.inner.process(fuzzer, state, executor)
        } else {
            // The main node does not process incoming events from the broker ATM
            self.inner.process(fuzzer, state, executor)
        }
    }

    fn on_shutdown(&mut self) -> Result<(), Error> {
        self.inner.on_shutdown()?;
        self.client.sender_mut().send_exiting()
    }
}

#[cfg(feature = "std")]
impl<EMH, OT, S, SP> CanSerializeObserver<OT> for CentralizedEventManager<EMH, S, SP>
where
    EMH: AdaptiveSerializer,
    SP: ShMemProvider,
    OT: Serialize + MatchNameRef,
{
    fn serialize_observers(&mut self, observers: &OT) -> Result<Option<Vec<u8>>, Error> {
        serialize_observers_adaptive::<EMH, S, OT>(self, observers, 2, 80)
    }
}

impl<EM, EMH, S, SP> ProgressReporter<S> for CentralizedEventManager<EM, EMH, SP>
where
    SP: ShMemProvider,
{
    fn maybe_report_progress(
        &mut self,
        state: &mut S,
        monitor_timeout: Duration,
    ) -> Result<(), Error> {
        default_maybe_report_progress(self, state, monitor_timeout)
    }

    fn report_progress(&mut self, state: &mut S) -> Result<(), Error> {
        default_report_progress(self, state)
    }
}

impl<EM, EMH, SP> HasEventManagerId for CentralizedEventManager<EM, EMH, SP>
where
    SP: ShMemProvider,
{
    fn mgr_id(&self) -> EventManagerId {
        self.inner.mgr_id()
    }
}

impl<EM, EMH, SP> CentralizedEventManager<EM, EMH, SP>
where
    SP: ShMemProvider,
{
    /// Describe the client event manager's LLMP parts in a restorable fashion
    pub fn describe(&self) -> Result<LlmpClientDescription, Error> {
        self.client.describe()
    }

    /// Write the config for a client [`EventManager`] to env vars, a new
    /// client can reattach using [`CentralizedEventManagerBuilder::build_existing_client_from_env()`].
    #[cfg(feature = "std")]
    pub fn to_env(&self, env_name: &str) {
        self.client.to_env(env_name).unwrap();
    }

    /// Know if this instance is main or secondary
    pub fn is_main(&self) -> bool {
        self.is_main
    }
}

impl<EM, EMH, SP> CentralizedEventManager<EM, EMH, SP>
where
    SP: ShMemProvider,
{
    #[cfg(feature = "llmp_compression")]
    fn forward_to_main<I>(&mut self, event: &Event<I>) -> Result<(), Error>
    where
        I: Input,
    {
        let serialized = postcard::to_allocvec(event)?;
        let flags = LLMP_FLAG_INITIALIZED;

        match self.compressor.maybe_compress(&serialized) {
            Some(comp_buf) => {
                self.client.send_buf_with_flags(
                    _LLMP_TAG_TO_MAIN,
                    flags | LLMP_FLAG_COMPRESSED,
                    &comp_buf,
                )?;
            }
            None => {
                self.client.send_buf(_LLMP_TAG_TO_MAIN, &serialized)?;
            }
        }
        Ok(())
    }

    #[cfg(not(feature = "llmp_compression"))]
    fn forward_to_main<I>(&mut self, event: &Event<I>) -> Result<(), Error>
    where
        I: Input,
    {
        let serialized = postcard::to_allocvec(event)?;
        self.client.send_buf(_LLMP_TAG_TO_MAIN, &serialized)?;
        Ok(())
    }

    fn receive_from_secondary<E, S, Z>(
        &mut self,
        fuzzer: &mut Z,
        state: &mut S,
        executor: &mut E,
    ) -> Result<usize, Error>
    where
        S: HasCorpus + Stoppable,
        <S::Corpus as Corpus>::Input: DeserializeOwned + Input,
        EMH: EventManagerHooksTuple<<S::Corpus as Corpus>::Input, S>,
        E: HasObservers,
        E::Observers: DeserializeOwned,
        EM: HasEventManagerId + EventFirer<<S::Corpus as Corpus>::Input, S>,
    {
        // TODO: Get around local event copy by moving handle_in_client
        let self_id = self.client.sender().id();
        let mut count = 0;
        while let Some((client_id, tag, _flags, msg)) = self.client.recv_buf_with_flags()? {
            assert!(
                tag == _LLMP_TAG_TO_MAIN,
                "Only _LLMP_TAG_TO_MAIN parcel should have arrived in the main node!"
            );

            if client_id == self_id {
                continue;
            }
            #[cfg(not(feature = "llmp_compression"))]
            let event_bytes = msg;
            #[cfg(feature = "llmp_compression")]
            let compressed;
            #[cfg(feature = "llmp_compression")]
            let event_bytes = if _flags & LLMP_FLAG_COMPRESSED == LLMP_FLAG_COMPRESSED {
                compressed = self.compressor.decompress(msg)?;
                &compressed
            } else {
                msg
            };
            let event: Event<<S::Corpus as Corpus>::Input> = postcard::from_bytes(event_bytes)?;
            log::debug!("Processor received message {}", event.name_detailed());
            self.handle_in_main(fuzzer, executor, state, client_id, event)?;
            count += 1;
        }
        Ok(count)
    }

    // Handle arriving events in the main node
    fn handle_in_main<E, S, Z>(
        &mut self,
        fuzzer: &mut Z,
        executor: &mut E,
        state: &mut S,
        client_id: ClientId,
        event: Event<<S::Corpus as Corpus>::Input>,
    ) -> Result<(), Error>
    where
        E: HasObservers,
        E::Observers: DeserializeOwned,
        S: HasCorpus + Stoppable,
        EMH: EventManagerHooksTuple<<S::Corpus as Corpus>::Input, S>,
        <S::Corpus as Corpus>::Input: Input,
        EM: HasEventManagerId + EventFirer<<S::Corpus as Corpus>::Input, S>,
    {
        log::debug!("handle_in_main!");

        let event_name = event.name_detailed();

        match event {
            Event::NewTestcase {
                input,
                client_config,
                exit_kind,
                corpus_size,
                observers_buf,
                time,
                executions,
                forward_id,
                #[cfg(feature = "multi_machine")]
                node_id,
            } => {
                log::debug!(
                    "Received {} from {client_id:?} ({client_config:?}, forward {forward_id:?})",
                    event_name
                );

                let res =
                    if client_config.match_with(&self.configuration()) && observers_buf.is_some() {
                        let observers: E::Observers =
                            postcard::from_bytes(observers_buf.as_ref().unwrap())?;
                        #[cfg(feature = "scalability_introspection")]
                        {
                            state.scalability_monitor_mut().testcase_with_observers += 1;
                        }
                        log::debug!(
                            "[{}] Running fuzzer with event {}",
                            process::id(),
                            event_name
                        );
                        fuzzer.evaluate_execution(
                            state,
                            self,
                            input.clone(),
                            &observers,
                            &exit_kind,
                            false,
                        )?
                    } else {
                        #[cfg(feature = "scalability_introspection")]
                        {
                            state.scalability_monitor_mut().testcase_without_observers += 1;
                        }
                        log::debug!(
                            "[{}] Running fuzzer with event {}",
                            process::id(),
                            event_name
                        );
                        fuzzer.evaluate_input_with_observers::<E>(
                            state,
                            executor,
                            self,
                            input.clone(),
                            false,
                        )?
                    };

                if let Some(item) = res.1 {
                    let event = Event::NewTestcase {
                        input,
                        client_config,
                        exit_kind,
                        corpus_size,
                        observers_buf,
                        time,
                        executions,
                        forward_id,
                        #[cfg(feature = "multi_machine")]
                        node_id,
                    };

                    self.hooks.on_fire_all(state, client_id, &event)?;

                    log::debug!(
                        "[{}] Adding received Testcase {} as item #{item}...",
                        process::id(),
                        event_name
                    );

                    self.inner.fire(state, event)?;
                } else {
                    log::debug!("[{}] {} was discarded...)", process::id(), event_name);
                }
            }
            Event::Stop => {
                state.request_stop();
            }
            _ => {
                return Err(Error::unknown(format!(
                    "Received illegal message that message should not have arrived: {:?}.",
                    event.name()
                )));
            }
        }

        Ok(())
    }
}

/*
impl<EM, SP> Drop for CentralizedEventManager<EM, SP>
where
    EM: UsesState,    SP: ShMemProvider + 'static,
{
    /// LLMP clients will have to wait until their pages are mapped by somebody.
    fn drop(&mut self) {
        self.await_restart_safe();
    }
}*/
