use crate::Cli;
use tangram_client as tg;
use tg::Handle as _;
use tokio::io::AsyncReadExt as _;

// Put a build.
#[derive(Clone, Debug, clap::Args)]
#[group(skip)]
pub struct Args {
	#[arg(long)]
	pub json: Option<String>,
}

impl Cli {
	pub async fn command_build_put(&self, args: Args) -> tg::Result<()> {
		let json = if let Some(json) = args.json {
			json
		} else {
			let mut json = String::new();
			tokio::io::stdin()
				.read_to_string(&mut json)
				.await
				.map_err(|source| tg::error!(!source, "failed to read stdin"))?;
			json
		};
		let arg: tg::build::put::Arg = serde_json::from_str(&json)
			.map_err(|source| tg::error!(!source, "failed to deseralize"))?;
		self.handle.put_build(&arg.id.clone(), arg.clone()).await?;
		println!("{}", arg.id);
		Ok(())
	}
}
