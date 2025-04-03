use server::start_server;
use std::error::Error;
use tokio::runtime::Builder;

mod server;

fn main() -> Result<(), Box<dyn Error + Sync + Send>> {
    match Builder::new_multi_thread()
        .worker_threads(8)
        .enable_all()
        .build()
    {
        Ok(runtime) => {
            println!("Tokio runtime successfully created");

            runtime.block_on(async {
                match start_server().await {
                    Ok(_) => {
                        println!("Start server successfully");
                    }
                    Err(err) => {
                        println!("Server error: {}", err);
                    }
                }
                match tokio::signal::ctrl_c().await {
                    Ok(_) => println!("Shutting downn server gracefully"),

                    Err(err) => println!("Can not shut down server! {}", err),
                }
            });
        }

        Err(err) => {
            eprintln!("Something went wrong! {}", err);
            std::process::exit(1);
        }
    }

    Ok(())
}
