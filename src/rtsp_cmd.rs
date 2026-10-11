//! The `rtsp` subcommand and the backend choice (SPEC §8, DESIGN §3.3)
//!
//! `--rtsp-backend`, else the config's `rtsp_backend`, else GStreamer when it is compiled in
//! (the beta default), else native. Choosing a backend that isn't compiled in is a startup
//! error.

use crate::common::NeoReactor;
use anyhow::{bail, Result};
use clap::{Parser, ValueEnum};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Backend {
    Gstreamer,
    Native,
}

impl std::str::FromStr for Backend {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "gstreamer" => Ok(Backend::Gstreamer),
            "native" => Ok(Backend::Native),
            o => bail!("rtsp_backend = {o:?}: use \"gstreamer\" or \"native\""),
        }
    }
}

/// The rtsp command will serve all cameras in the config over the rtsp protocol
#[derive(Parser, Debug, Default)]
pub struct Opt {
    /// gstreamer or native (default: the config's rtsp_backend, else gstreamer if built in)
    #[arg(long, value_enum)]
    pub(crate) rtsp_backend: Option<Backend>,
}

/// The backend to run
pub(crate) fn choose(cli: Option<Backend>, config: Option<&str>) -> Result<Backend> {
    let wanted = match (cli, config) {
        (Some(b), _) => b,
        (None, Some(c)) => c.parse()?,
        (None, None) if cfg!(feature = "gstreamer") => Backend::Gstreamer,
        (None, None) => Backend::Native,
    };
    match wanted {
        Backend::Gstreamer if !cfg!(feature = "gstreamer") => {
            bail!("The GStreamer RTSP backend is not compiled into this build; use rtsp_backend = \"native\"")
        }
        Backend::Native if !cfg!(feature = "native-rtsp") => {
            bail!("The native RTSP backend is not compiled into this build (feature native-rtsp); use rtsp_backend = \"gstreamer\"")
        }
        b => Ok(b),
    }
}

pub(crate) async fn main(opt: Opt, reactor: NeoReactor) -> Result<()> {
    let config = reactor.config().await?.borrow().clone();
    let backend = choose(opt.rtsp_backend, config.rtsp_backend.as_deref())?;
    log::info!("RTSP backend: {backend:?}");
    match backend {
        #[cfg(feature = "gstreamer")]
        Backend::Gstreamer => crate::rtsp::main(crate::rtsp::Opt {}, reactor).await,
        #[cfg(feature = "native-rtsp")]
        Backend::Native => crate::rtsp_native::main(reactor).await,
        #[allow(unreachable_patterns)]
        _ => unreachable!("choose() rejects backends that are not compiled in"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_choose() {
        assert_eq!(
            choose(Some(Backend::Native), Some("gstreamer")).ok(),
            cfg!(feature = "native-rtsp").then_some(Backend::Native)
        );
        assert!(choose(None, Some("nope")).is_err());
        let default = choose(None, None).unwrap();
        if cfg!(feature = "gstreamer") {
            assert_eq!(default, Backend::Gstreamer);
        } else {
            assert_eq!(default, Backend::Native);
        }
    }
}
