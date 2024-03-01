use crate::Cli;
use tangram_client as tg;
use tangram_error::{Result, WrapErr};

/// Generate documentation.
#[derive(Debug, clap::Args)]
#[command(verbatim_doc_comment)]
pub struct Args {
	/// If this flag is set, the package's lockfile will not be updated.
	#[arg(long)]
	pub locked: bool,

	#[arg(short, long, default_value = ".")]
	pub package: tg::Dependency,

	/// Generate the documentation for the runtime.
	#[arg(short, long, default_value = "false")]
	pub runtime: bool,
}

impl Cli {
	pub async fn command_doc(&self, args: Args) -> Result<()> {
		let client = &self.client().await?;

		// Create the language server.
		let server = tangram_server::language::Server::new(client, tokio::runtime::Handle::current());

		let module = if args.runtime {
			let path: tg::Path = "tangram.d.ts".parse().unwrap();
			tangram_server::language::Module::Library(tangram_server::language::module::Library {
				path: path.clone(),
			})
		} else {
			let (package, lock) = tg::package::get_with_lock(client, &args.package).await?;
			let path = tg::package::get_root_module_path(client, &package).await?;
			let package = package.id(client).await?.clone();
			let lock = lock.id(client).await?.clone();
			tangram_server::language::Module::Normal(tangram_server::language::module::Normal {
				package,
				path: path.clone(),
				lock,
			})
		};

		// Get the docs.
		let docs = server.docs(&module).await?;

		// Serialize the docs to json.
		let json = serde_json::to_string_pretty(&docs).wrap_err("Failed to serialize the docs.")?;

		// Print the json.
		println!("{json}");

		Ok(())
	}
}
