///
/// # Neolink Battery
///
/// This module handles the printing of the Battery status
/// in xml format
///
/// # Usage
///
/// ```bash
/// neolink battery --config=config.toml CameraName
/// ```
///
use anyhow::{Context, Result};

mod cmdline;

use crate::common::{NeoReactor, StateFile};
use neolink_core::bc::xml::BatteryInfo;

pub(crate) use cmdline::Opt;

/// Entry point for the battery subcommand
///
/// Opt is the command line options
pub(crate) async fn main(opt: Opt, reactor: NeoReactor) -> Result<()> {
    let camera = reactor.get(&opt.camera).await?;

    if !opt.fresh {
        let config = camera.config().await?.borrow().clone();
        // The last reading from a wake (SPEC P7, P15); reading it doesn't wake the camera
        if let Some(b) = StateFile::for_camera(&config).load().last_battery {
            eprintln!(
                "{}: reading from {} s ago (use --fresh to wake the camera)",
                opt.camera,
                b.age()
            );
            print_xml(&b.to_info(config.channel_id));
            return Ok(());
        }
        eprintln!("{}: no saved reading yet, waking the camera", opt.camera);
    }

    let state = camera
        .run_task_for("cli:battery", |cam| {
            Box::pin(async move {
                cam.battery_info()
                    .await
                    .context("Unable to get camera Battery state")
            })
        })
        .await?;
    print_xml(&state);
    Ok(())
}

fn print_xml(state: &BatteryInfo) {
    let ser = String::from_utf8({
        let mut buf = bytes::BytesMut::new();
        quick_xml::se::to_writer(&mut buf, state).expect("Should Ser the struct");
        buf.to_vec()
    })
    .expect("Should be UTF8");
    println!("{}", ser);
}
