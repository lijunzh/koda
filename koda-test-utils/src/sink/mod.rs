//! [`TestSink`], compiled from koda-core's own file so the two can't drift:
//! koda-core's unit tests build the same file under `#[cfg(test)]`.

use koda_core::engine::EngineEvent;
use koda_core::engine::sink::EngineSink;

#[path = "../../../koda-core/src/engine/test_sink.rs"]
mod test_sink;

pub use test_sink::TestSink;
