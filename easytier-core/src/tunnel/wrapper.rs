use std::{
    any::Any,
    pin::Pin,
    sync::{Arc, Mutex},
};

use crate::{foundation::bandwidth::TransmissionWindowSource, proto::common::TunnelInfo};

use super::{Tunnel, ZCPacketSink, ZCPacketStream};

pub struct TunnelWrapper<R, W> {
    reader: Arc<Mutex<Option<R>>>,
    writer: Arc<Mutex<Option<W>>>,
    info: Option<TunnelInfo>,
    _associate_data: Option<Box<dyn Any + Send + 'static>>,
    bandwidth_source: Option<Arc<dyn TransmissionWindowSource>>,
}

impl<R, W> TunnelWrapper<R, W> {
    pub fn new(reader: R, writer: W, info: Option<TunnelInfo>) -> Self {
        Self::new_with_associate_data(reader, writer, info, None)
    }

    pub fn new_with_associate_data(
        reader: R,
        writer: W,
        info: Option<TunnelInfo>,
        associate_data: Option<Box<dyn Any + Send + 'static>>,
    ) -> Self {
        Self {
            reader: Arc::new(Mutex::new(Some(reader))),
            writer: Arc::new(Mutex::new(Some(writer))),
            info,
            _associate_data: associate_data,
            bandwidth_source: None,
        }
    }

    pub fn with_bandwidth_source(
        mut self,
        source: Option<Arc<dyn TransmissionWindowSource>>,
    ) -> Self {
        self.bandwidth_source = source;
        self
    }
}

impl<R, W> Tunnel for TunnelWrapper<R, W>
where
    R: ZCPacketStream + Send + 'static,
    W: ZCPacketSink + Send + 'static,
{
    fn split(&self) -> (Pin<Box<dyn ZCPacketStream>>, Pin<Box<dyn ZCPacketSink>>) {
        let reader = self.reader.lock().unwrap().take().unwrap();
        let writer = self.writer.lock().unwrap().take().unwrap();
        (Box::pin(reader), Box::pin(writer))
    }

    fn info(&self) -> Option<TunnelInfo> {
        self.info.clone()
    }

    fn bandwidth_source(&self) -> Option<Arc<dyn TransmissionWindowSource>> {
        self.bandwidth_source.clone()
    }
}
