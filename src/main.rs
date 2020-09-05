// `error_chain!` can recurse deeply
#![recursion_limit = "1024"]

#[macro_use]
extern crate error_chain;
extern crate getopts;

mod hpk;

// We'll put our errors in an `errors` module, and other modules in
// this crate will `use errors::*;` to get access to everything
// `error_chain!` creates.
mod errors {
    // Create the Error, ErrorKind, ResultExt, and Result types
    error_chain! {
        foreign_links {
            Fmt(::std::fmt::Error);
            Io(::std::io::Error) #[cfg(unix)];
        }
    }
}

use errors::*;

use hpk::Archive;
use hpk::Directory;
use std::iter::Peekable;
use std::slice::Iter;

struct DirCtx<'a> {
    dir: &'a Directory,
    iter: Peekable<Iter<'a, Directory>>,
}

fn main() {
    if let Err(ref e) = run() {
        use std::io::Write;
        let stderr = &mut ::std::io::stderr();
        let errmsg = "Error writing to stderr";

        writeln!(stderr, "error: {}", e).expect(errmsg);

        for e in e.iter().skip(1) {
            writeln!(stderr, "caused by: {}", e).expect(errmsg);
        }

        if let Some(backtrace) = e.backtrace() {
            writeln!(stderr, "backtrace: {:?}", backtrace).expect(errmsg);
        }

        ::std::process::exit(1);
    }
}

fn build_path(dir: &Directory, dirstack: &Vec<DirCtx>) -> String {
    let mut path = String::new();
    for ctx in dirstack {
        if let Some(n) = ctx.dir.name() {
            path.push_str(n);
            path.push(::std::path::MAIN_SEPARATOR);
        };
    }
    if let Some(n) = dir.name() {
        path.push_str(n);
        path.push(::std::path::MAIN_SEPARATOR);
    };
    path
}

fn foreach_dir_in_dir<F>(_archive: &Archive, dir: &Directory, closure: F) -> Result<()>
where
    F: Fn(&Directory, &str, u16) -> Result<()>,
{
    // Initial state
    let mut dirstack: Vec<DirCtx> = Vec::new();
    let mut ctx = DirCtx {
        dir: dir,
        iter: dir.directories().iter().peekable(),
    };

    // Process root directory
    closure(
        ctx.dir,
        &build_path(ctx.dir, &dirstack),
        dirstack.len() as u16,
    )?;

    while !dirstack.is_empty() || !ctx.iter.peek().is_none() {
        let next_dir = ctx.iter.next();
        match next_dir {
            None => {
                /* Last directory for this level processed, resume to where we left off in
                 * the parent directory. */
                ctx = dirstack.pop().unwrap();
            }
            Some(d) => {
                dirstack.push(ctx);
                ctx = DirCtx {
                    dir: d,
                    iter: d.directories().iter().peekable(),
                };
                closure(
                    ctx.dir,
                    &build_path(ctx.dir, &dirstack),
                    dirstack.len() as u16,
                )?;
            }
        };
    }
    Ok(())
}

fn foreach_file_in_dir<F>(archive: &Archive, dir: &Directory, closure: F) -> Result<()>
where
    F: Fn(&hpk::File, &str, u16) -> Result<()>,
{
    foreach_dir_in_dir(archive, dir, |dir, path, level| {
        for f in dir.files() {
            closure(f, path, level)?;
        }
        Ok(())
    })
}

fn list_archive(archive: &Archive) -> Result<()> {
    foreach_file_in_dir(archive, archive.root_directory(), |file, path, _level| {
        let mut display_path = String::new();
        println!("{}{}", path, file.name());
        unimplemented!();
        Ok(())
    })
}

/* Create all the output directory hiererchy under a specified path. */
fn create_dirs(archive: &Archive, directory: &Directory, outpath: &str) -> Result<()> {
    use std::fs::DirBuilder;
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    foreach_dir_in_dir(archive, directory, |_dir, path, _level| {
        let mut dirpath = String::from(outpath);
        dirpath.push(std::path::MAIN_SEPARATOR);
        dirpath.push_str(path);
        builder.create(dirpath)?;
        Ok(())
    })?;
    Ok(())
}

/* Extract a single file to a specified output directory */
fn extract_file(archive: &Archive, file: &hpk::File, outpath: &str) -> Result<()> {
    let mut data = archive.file_data(file)?;
    let mut out;
    let mut remain = data.size() as usize;
    {
        use std::fs::File;
        let mut filepath = String::new();
        filepath.push_str(outpath);
        filepath.push_str(file.name());
        out = File::create(filepath)?;
    }

    while remain > 0 {
        use std::io::Read;
        use std::io::Write;
        // XXX: There must be a faster way
        let mut buf = vec![0; 0x100000];
        let buflen = buf.len();
        let size = if remain > buflen { buflen } else { remain };
        data.read_exact(&mut buf[0..size])?;
        out.write(&buf[0..size])?;
        remain -= size;
    }
    Ok(())
}

fn extract_archive(archive: &Archive, outpath: &str) -> Result<()> {
    let rootdir = archive.root_directory();
    create_dirs(archive, rootdir, outpath)?;
    foreach_file_in_dir(archive, archive.root_directory(), |file, path, _level| {
        let mut filepath = String::new();
        filepath.push_str(outpath);
        filepath.push(std::path::MAIN_SEPARATOR);
        filepath.push_str(path);
        println!("{}{}", filepath, file.name());
        extract_file(archive, file, &filepath)?;
        Ok(())
    })
}

fn run() -> Result<()> {
    use getopts::Options;

    let args: Vec<String> = std::env::args().collect();
    let mut opts = Options::new();
    let matches = opts.parse(&args[1..]).unwrap();
    if matches.free.len() != 2 {
        bail!(
            "Incorrect number of arguments. Expected 2, got {}.",
            matches.free.len()
        );
    }

    let archive = Archive::open(&matches.free[0]).chain_err(|| "Unable to open archive")?;
    let rootdir = archive.root_directory();
    println!("Num files: {}", rootdir.files().len());
    println!("Num directories: {}", rootdir.directories().len());

    //list_archive(&archive);
    extract_archive(&archive, &matches.free[1])?;

    Ok(())
}
