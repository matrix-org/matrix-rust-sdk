use std::{env::temp_dir, path::Path, sync::Arc, time::Duration};

use anyhow::Result;
use matrix_sdk_crypto::store::{locks::CryptoStoreLock, DynCryptoStore, IntoCryptoStore as _};
use matrix_sdk_sqlite::SqliteCryptoStore;
use nix::{
    libc::rand,
    sys::wait::wait,
    unistd::{close, fork, pipe, read, write},
};
use speedy::{Readable, Writable};
use tokio::{runtime::Runtime, time::sleep};

#[derive(Clone, Debug, Writable, Readable)]
enum Command {
    WriteValue(String),
    ReadValue(String),
    Quit,
}

impl Command {
    async fn assert(&self, store: &Arc<DynCryptoStore>) -> Result<()> {
        match self {
            Command::WriteValue(written) => {
                // The child must have written the value.
                let read = store.get_custom_value(KEY).await?;
                assert_eq!(read, Some(written.as_bytes().to_vec()));
            }
            Command::ReadValue(_read) => {
                // The child removes the value after validating it.
                let read = store.get_custom_value(KEY).await?;
                assert_eq!(read, None);
            }
            Command::Quit => {}
        }
        Ok(())
    }
}

fn write_command(fd: i32, command: Command) -> Result<()> {
    eprintln!("parent send: {command:?}");
    let serialized = command.write_to_vec()?;
    let len = write(fd, &serialized)?;
    assert_eq!(len, serialized.len());
    Ok(())
}

fn read_command(fd: i32) -> Result<Command> {
    let mut buf = vec![0; 1024]; // 1024 bytes enough for anyone
    read(fd, &mut buf)?;
    let command = Command::read_from_buffer(&buf)?;
    eprintln!("child received: {command:?}");
    Ok(command)
}

const LOCK_KEY: &str = "lock_key";
const KEY: &str = "custom_key";
const GENERATION_KEY: &str = "generation";

struct CloseChildGuard {
    write_pipe: i32,
}

impl Drop for CloseChildGuard {
    fn drop(&mut self) {
        write_command(self.write_pipe, Command::Quit).unwrap();

        let status = wait().unwrap();
        eprintln!("Child status: {status:?}");

        let _ = close(self.write_pipe);
    }
}

async fn parent_main(path: &Path, write_pipe: i32) -> Result<()> {
    let store = SqliteCryptoStore::open(path, None).await?.into_crypto_store();
    let lock_key = LOCK_KEY.to_string();

    let _close_child_guard = CloseChildGuard { write_pipe };

    let mut generation = 0;

    // Set initial generation to 0.
    store.set_custom_value(GENERATION_KEY, vec![generation]).await?;

    let mut lock = CryptoStoreLock::new(store.clone(), lock_key.clone(), "parent".to_owned(), None);

    loop {
        // Write a command.
        let val = unsafe { rand() } % 2;

        let id = unsafe { rand() };
        let str = format!("hello {id}");

        let cmd = match val {
            0 => {
                // the child will write, so we remove the value
                let cmd = Command::WriteValue(str);

                store.remove_custom_value(KEY).await?;

                write_command(write_pipe, cmd.clone())?;
                cmd
            }

            1 => {
                // the child will read, so we write the value
                store.set_custom_value(KEY, str.as_bytes().to_vec()).await?;

                let cmd = Command::ReadValue(str);
                write_command(write_pipe, cmd.clone())?;
                cmd
            }

            _ => unreachable!(),
        };

        loop {
            // Compete with the child to take the lock!
            lock.lock().await?;

            let read_generation =
                store.get_custom_value(GENERATION_KEY).await?.expect("there's always a generation")
                    [0];

            // Check that if we've seen the latest result, based on the generation number
            // (any write to the generation indicates somebody else wrote to the
            // database).
            if generation != read_generation {
                // Run assertions here.
                cmd.assert(&store).await?;

                generation = read_generation;

                lock.unlock().await?;
                break;
            }

            println!("parent: waiting...");
            lock.unlock().await?;

            sleep(Duration::from_millis(1)).await;
        }
    }

    #[allow(unreachable_code)]
    Ok(())
}

async fn child_main(path: &Path, read_pipe: i32) -> Result<()> {
    let store = SqliteCryptoStore::open(path, None).await?.into_crypto_store();
    let lock_key = LOCK_KEY.to_string();

    let mut lock = CryptoStoreLock::new(store.clone(), lock_key.clone(), "child".to_owned(), None);

    loop {
        eprintln!("child waits for command");
        match read_command(read_pipe)? {
            Command::WriteValue(val) => {
                eprintln!("child received command: write {val}; waiting for lock");

                lock.lock().await?;

                eprintln!("child got the lock");

                store.set_custom_value(KEY, val.as_bytes().to_vec()).await?;

                let generation = store.get_custom_value(GENERATION_KEY).await?.unwrap()[0];
                let generation = generation.wrapping_add(1);
                store.set_custom_value(GENERATION_KEY, vec![generation]).await?;

                lock.unlock().await?;
            }

            Command::ReadValue(expected) => {
                eprintln!("child received command: read {expected}; waiting for lock");

                lock.lock().await?;

                eprintln!("child got the lock");

                let val = store.get_custom_value(KEY).await?.expect("missing value in child");
                assert_eq!(val, expected.as_bytes());

                store.remove_custom_value(KEY).await?;

                let generation = store.get_custom_value(GENERATION_KEY).await?.unwrap()[0];
                store.set_custom_value(GENERATION_KEY, vec![generation + 1]).await?;

                lock.unlock().await?;
            }

            Command::Quit => {
                break;
            }
        }
    }

    close(read_pipe)?;

    Ok(())
}

fn main() -> Result<()> {
    let path = temp_dir().join("db.sqlite");

    if path.exists() {
        std::fs::remove_dir_all(&path)?;
    }

    // Force running migrations first.
    {
        let rt = Runtime::new()?;
        let path = path.clone();
        rt.block_on(async {
            let _store = SqliteCryptoStore::open(path, None).await?;
            anyhow::Ok(())
        })?;
    }

    let (read_pipe, write_pipe) = pipe()?;

    let pid = unsafe { fork() }?;
    match pid {
        nix::unistd::ForkResult::Parent { .. } => {
            // Parent doesn't read.
            close(read_pipe)?;

            let rt = Runtime::new()?;
            rt.block_on(parent_main(&path, write_pipe))
        }

        nix::unistd::ForkResult::Child => {
            // Child doesn't write.
            close(write_pipe)?;

            let rt = Runtime::new()?;
            rt.block_on(child_main(&path, read_pipe))
        }
    }
}
