use std::borrow::Cow;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use goblin::Object;

mod args;
mod decode;
mod demangler;
mod replace;

#[macro_export]
macro_rules! exit {
    () => {{
        std::process::exit(0);
    }};

    ($($arg:tt)*) => {{
        eprintln!($($arg)*);
        std::process::exit(1);
    }};
}

#[macro_export]
macro_rules! assert_exit {
    ($cond:expr $(,)?) => {{
        if !($cond) {
            $crate::exit!();
        }
    }};

    ($cond:expr, $($arg:tt)+) => {{
        if !($cond) {
            $crate::exit!($($arg)*);
        }
    }};
}

struct GenericBinary<'a> {
    symbols: Vec<&'a str>,
    libs: Vec<&'a str>,
    raw: &'a [u8],
}

fn demangle_line<'a>(args: &args::Cli, s: &'a str, config: &replace::Config) -> Cow<'a, str> {
    let mut left = 0;
    for idx in 0..s.len() {
        if s.as_bytes()[idx] == b'<' {
            left = idx;
            break;
        }
    }

    let mut right = 0;
    for idx in 0..s.len() {
        if s[left..].as_bytes()[idx] == b'>' {
            right = left + idx;
            break;
        }
    }

    for idx in left..right {
        if s.as_bytes()[idx] == b'+' {
            right = idx;
            break;
        }
    }

    if left == 0 || right == 0 {
        return Cow::Borrowed(s);
    }

    let mangled = &s[left + 1..=right - 1];
    let demangled = match demangler::Symbol::parse_with_config(mangled, &config) {
        Ok(demangled) => Cow::Owned(demangled.display()),
        Err(..) => {
            if let Some("__Z") = mangled.get(0..3) {
                Cow::Owned(format!("{}", rustc_demangle::demangle(mangled)))
            } else {
                Cow::Borrowed(mangled)
            }
        }
    };

    // let demangled = Cow::Owned(format!("{:#}", demangle(&s[left + 1..=right - 1])));
    let demangled = if args.simplify { replace::simplify_type(&demangled) } else { demangled };

    Cow::Owned(s[..=left].to_string() + demangled.as_ref() + &s[right..])
}

fn objdump(args: &args::Cli, config: &replace::Config) {
    let objdump = Command::new("objdump")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .arg("-x86-asm-syntax=intel")
        .arg("-D")
        .arg(&args.path)
        .spawn()
        .unwrap();

    let mut stdout = BufReader::new(objdump.stdout.unwrap());
    for line in (&mut stdout).lines() {
        let line = match line {
            Ok(ref line) => demangle_line(&args, line, config),
            Err(_) => Cow::Borrowed("???????????"),
        };

        println!("{line}");
    }
}

// TODO: impliment own version of `objdump`.
fn main() -> goblin::error::Result<()> {
    use demangler::Error;

    let args = args::Cli::parse();
    let config = replace::Config::from_env(&args);

    let object_bytes = std::fs::read(&args.path).unwrap();
    let object = goblin::Object::parse(object_bytes.as_slice())?;
    let object = match object {
        Object::Mach(bin) => {
            let bin = match bin {
                goblin::mach::Mach::Fat(fat) => fat.get(0)?,
                goblin::mach::Mach::Binary(bin) => bin,
            };

            let (_section, raw) = bin
                .segments
                .into_iter()
                .find(|seg| matches!(seg.name(), Ok("__TEXT")))
                .expect("Object is missing a `text` section")
                .sections()
                .expect("Failed to parse section")
                .into_iter()
                .find(|(sec, _)| matches!(sec.name(), Ok("__text")))
                .unwrap_or_else(|| exit!("Object looks like it's been stripped"));

            GenericBinary {
                symbols: bin.symbols().filter_map(|x| x.map(|y| y.0).ok()).collect(),
                libs: bin.libs,
                raw,
            }
        }
        Object::Elf(bin) => {
            let raw = bin
                .section_headers
                .into_iter()
                .find(|header| &bin.shdr_strtab[header.sh_name] == ".text")
                .and_then(|header| header.file_range())
                .map(|section_range| &object_bytes[section_range])
                .unwrap_or_else(|| exit!("No text section found"));

            GenericBinary { symbols: bin.strtab.to_vec()?, libs: bin.libraries, raw }
        }
        Object::Unknown(..) => exit!("Unable to recognize the object's format"),
        _ => todo!(),
    };

    if args.libs {
        println!("{}:", args.path.display());
        for lib in object.libs.iter().skip(1) {
            let lib = std::path::Path::new(lib);
            let lib_name =
                lib.file_name().map(|v| v.to_str().unwrap()).unwrap_or("???? Invalid utf8");

            println!("\t{} => {}", lib_name, lib.display());
        }

        exit!();
    }

    if args.names {
        let symbols: Vec<&str> = object.symbols;
        let thread_count = std::thread::available_parallelism().unwrap_or_else(|err| {
            eprintln!("Failed to get thread_count: {err}");
            unsafe { std::num::NonZeroUsize::new_unchecked(1) }
        });

        let symbols_per_thread = (symbols.len() + (thread_count.get() - 1)) / thread_count;
        let mut handles = Vec::with_capacity(thread_count.get());

        for symbols_chunk in symbols.chunks(symbols_per_thread) {
            // FIXME: use thread::scoped when it becomes stable to replace this.
            // SAFETY: `symbols` is only dropped after the threads have joined therefore
            // it's safe to send to other threads as a &'static str.
            let symbols_chunk: &[&'static str] = unsafe {
                &*(symbols_chunk as *const [&str] as *const [*const str] as *const [&'static str])
            };

            handles.push(std::thread::spawn(move || {
                for symbol in symbols_chunk.iter().filter(|symbol| !symbol.is_empty()) {
                    // TODO: Simplify symbol here.

                    let demangled_name = match demangler::Symbol::parse(symbol) {
                        Ok(sym) => sym.display(),
                        Err(Error::UnknownPrefix) => rustc_demangle::demangle(symbol).to_string(),
                        Err(..) => symbol.to_string(),
                    };

                    println!("{demangled_name}");
                }
            }))
        }

        for handle in handles {
            let id = handle.thread().id();

            handle.join().unwrap_or_else(|e| {
                panic!("Failed to join thread with id: {:?}, error: {:?}", id, e)
            });
        }
    }

    if args.disassemble {
        objdump(&args, &config);
        todo!("{:?}", decode::x86_64::asm(decode::BitWidth::U64, &[0xf3, 0x48, 0xa5]));
    }

    Ok(())
}
