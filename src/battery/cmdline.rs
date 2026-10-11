use clap::Parser;

/// The battery command will dump the battery status to XML
///
/// By default it prints the reading taken during the camera's last wake, without waking it
#[derive(Parser, Debug)]
pub struct Opt {
    /// The name of the camera. Must be a name in the config
    pub camera: String,
    /// Wake the camera for a new reading
    #[arg(long)]
    pub fresh: bool,
}
