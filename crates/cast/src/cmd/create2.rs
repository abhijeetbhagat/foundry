use alloy_primitives::{Address, B256, U256, hex, keccak256};
use clap::Parser;
use eyre::{Result, WrapErr};
use rand::{RngCore, SeedableRng, rngs::StdRng};
use regex::RegexSetBuilder;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

// https://etherscan.io/address/0x4e59b44847b379578588920ca78fbf26c0b4956c#code
const DEPLOYER: &str = "0x4e59b44847b379578588920ca78fbf26c0b4956c";

/// CLI arguments for `cast create2`.
#[derive(Clone, Debug, Parser)]
pub struct Create2Args {
    /// Prefix for the contract address.
    #[arg(
        long,
        short,
        required_unless_present_any = &["ends_with", "matching", "salt"],
        value_name = "HEX"
    )]
    starts_with: Option<String>,

    /// Suffix for the contract address.
    #[arg(long, short, value_name = "HEX")]
    ends_with: Option<String>,

    /// Sequence that the address has to match.
    #[arg(long, short, value_name = "HEX")]
    matching: Option<String>,

    /// Case sensitive matching.
    #[arg(short, long)]
    case_sensitive: bool,

    /// Address of the contract deployer.
    #[arg(
        short,
        long,
        default_value = DEPLOYER,
        value_name = "ADDRESS"
    )]
    deployer: Address,

    /// Salt to be used for the contract deployment. This option separate from the default salt
    /// mining with filters.
    #[arg(
        long,
        conflicts_with_all = [
            "starts_with",
            "ends_with",
            "matching",
            "case_sensitive",
            "caller",
            "seed",
            "no_random"
        ],
        value_name = "HEX"
    )]
    salt: Option<String>,

    /// Init code of the contract to be deployed.
    #[arg(short, long, value_name = "HEX")]
    init_code: Option<String>,

    /// Init code hash of the contract to be deployed.
    #[arg(alias = "ch", long, value_name = "HASH", required_unless_present = "init_code")]
    init_code_hash: Option<String>,

    /// Number of threads to use. Specifying 0 defaults to the number of logical cores.
    #[arg(global = true, long, short = 'j', visible_alias = "jobs")]
    threads: Option<usize>,

    /// Address of the caller. Used for the first 20 bytes of the salt.
    #[arg(long, value_name = "ADDRESS")]
    caller: Option<Address>,

    /// The random number generator's seed, used to initialize the salt.
    #[arg(long, value_name = "HEX")]
    seed: Option<B256>,

    /// Don't initialize the salt with a random value, and instead use the default value of 0.
    #[arg(long, conflicts_with = "seed")]
    no_random: bool,
}

pub struct Create2Output {
    pub address: Address,
    pub salt: B256,
}

impl Create2Args {
    pub fn run(self) -> Result<Create2Output> {
        let Self {
            starts_with,
            ends_with,
            matching,
            case_sensitive,
            deployer,
            salt,
            init_code,
            init_code_hash,
            threads,
            caller,
            seed,
            no_random,
        } = self;

        let init_code_hash = if let Some(init_code_hash) = init_code_hash {
            hex::FromHex::from_hex(init_code_hash)
        } else if let Some(init_code) = init_code {
            hex::decode(init_code).map(keccak256)
        } else {
            unreachable!();
        }?;

        if let Some(salt) = salt {
            let salt = hex::FromHex::from_hex(salt)?;
            let address = deployer.create2(salt, init_code_hash);
            sh_println!("{address}")?;
            return Ok(Create2Output { address, salt });
        }

        let mut regexs = vec![];

        if let Some(matches) = matching {
            if starts_with.is_some() || ends_with.is_some() {
                eyre::bail!("Either use --matching or --starts/ends-with");
            }

            let matches = matches.trim_start_matches("0x");

            if matches.len() != 40 {
                eyre::bail!("Please provide a 40 characters long sequence for matching");
            }

            hex::decode(matches.replace('X', "0")).wrap_err("invalid matching hex provided")?;
            // replacing X placeholders by . to match any character at these positions

            regexs.push(matches.replace('X', "."));
        }

        if let Some(prefix) = starts_with {
            regexs.push(format!(
                r"^{}",
                get_regex_hex_string(prefix).wrap_err("invalid prefix hex provided")?
            ));
        }
        if let Some(suffix) = ends_with {
            regexs.push(format!(
                r"{}$",
                get_regex_hex_string(suffix).wrap_err("invalid prefix hex provided")?
            ))
        }

        debug_assert!(
            regexs.iter().map(|p| p.len() - 1).sum::<usize>() <= 40,
            "vanity patterns length exceeded. cannot be more than 40 characters",
        );

        let regex = RegexSetBuilder::new(regexs).case_insensitive(!case_sensitive).build()?;

        let mut n_threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        if let Some(threads) = threads {
            n_threads = n_threads.min(threads);
        }
        if cfg!(test) {
            n_threads = n_threads.min(2);
        }

        let mut salt = B256::ZERO;
        let remaining = if let Some(caller_address) = caller {
            salt[..20].copy_from_slice(&caller_address.into_array());
            &mut salt[20..]
        } else {
            &mut salt[..]
        };

        if !no_random {
            let mut rng = match seed {
                Some(seed) => StdRng::from_seed(seed.0),
                None => StdRng::from_os_rng(),
            };
            rng.fill_bytes(remaining);
        }

        sh_println!("Configuration:")?;
        sh_println!("Init code hash: {init_code_hash}")?;
        sh_println!("Regex patterns: {:?}\n", regex.patterns())?;
        sh_println!(
            "Starting to generate deterministic contract address with {n_threads} threads..."
        )?;
        let mut handles = Vec::with_capacity(n_threads);
        let found = Arc::new(AtomicBool::new(false));
        let timer = Instant::now();

        // Loops through all possible salts in parallel until a result is found.
        // Each thread iterates over `(i..).step_by(n_threads)`.
        for i in 0..n_threads {
            // Create local copies for the thread.
            let increment = n_threads;
            let regex = regex.clone();
            let regex_len = regex.patterns().len();
            let found = Arc::clone(&found);
            handles.push(std::thread::spawn(move || {
                // Read the first bytes of the salt as a usize to be able to increment it.
                struct B256Aligned(B256, [usize; 0]);
                let mut salt = B256Aligned(salt, []);
                // SAFETY: B256 is aligned to `usize`.
                let salt_word = unsafe {
                    &mut *salt.0.as_mut_ptr().add(32 - usize::BITS as usize / 8).cast::<usize>()
                };
                // Important: add the thread index to the salt to avoid duplicate results.
                *salt_word = salt_word.wrapping_add(i);

                let mut checksum = [0; 42];
                loop {
                    // Stop if a result was found in another thread.
                    if found.load(Ordering::Relaxed) {
                        break None;
                    }

                    // Calculate the `CREATE2` address.
                    #[expect(clippy::needless_borrows_for_generic_args)]
                    let addr = deployer.create2(&salt.0, &init_code_hash);

                    // Check if the regex matches the calculated address' checksum.
                    let _ = addr.to_checksum_raw(&mut checksum, None);
                    // SAFETY: stripping 2 ASCII bytes ("0x") off of an already valid UTF-8 string
                    // is safe.
                    let s = unsafe { std::str::from_utf8_unchecked(checksum.get_unchecked(2..)) };
                    if regex.matches(s).into_iter().count() == regex_len {
                        // Notify other threads that we found a result.
                        found.store(true, Ordering::Relaxed);
                        break Some((addr, salt.0));
                    }

                    // Increment the salt for the next iteration.
                    *salt_word = salt_word.wrapping_add(increment);
                }
            }));
        }

        let results = handles.into_iter().filter_map(|h| h.join().unwrap()).collect::<Vec<_>>();
        let (address, salt) = results.into_iter().next().unwrap();
        sh_println!("Successfully found contract address in {:?}", timer.elapsed())?;
        sh_println!("Address: {address}")?;
        sh_println!("Salt: {salt} ({})", U256::from_be_bytes(salt.0))?;

        Ok(Create2Output { address, salt })
    }
}

fn get_regex_hex_string(s: String) -> Result<String> {
    let s = s.strip_prefix("0x").unwrap_or(&s);
    let pad_width = s.len() + s.len() % 2;
    hex::decode(format!("{s:0<pad_width$}"))?;
    Ok(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, b256};
    use std::str::FromStr;

    #[test]
    fn basic_create2() {
        let mk_args = |args: &[&str]| {
            Create2Args::parse_from(["foundry-cli", "--init-code-hash=0x0000000000000000000000000000000000000000000000000000000000000000"].iter().chain(args))
        };

        // even hex chars
        let args = mk_args(&["--starts-with", "aa"]);
        let create2_out = args.run().unwrap();
        assert!(format!("{:x}", create2_out.address).starts_with("aa"));

        let args = mk_args(&["--ends-with", "bb"]);
        let create2_out = args.run().unwrap();
        assert!(format!("{:x}", create2_out.address).ends_with("bb"));

        // odd hex chars
        let args = mk_args(&["--starts-with", "aaa"]);
        let create2_out = args.run().unwrap();
        assert!(format!("{:x}", create2_out.address).starts_with("aaa"));

        let args = mk_args(&["--ends-with", "bbb"]);
        let create2_out = args.run().unwrap();
        assert!(format!("{:x}", create2_out.address).ends_with("bbb"));

        // even hex chars with 0x prefix
        let args = mk_args(&["--starts-with", "0xaa"]);
        let create2_out = args.run().unwrap();
        assert!(format!("{:x}", create2_out.address).starts_with("aa"));

        // odd hex chars with 0x prefix
        let args = mk_args(&["--starts-with", "0xaaa"]);
        let create2_out = args.run().unwrap();
        assert!(format!("{:x}", create2_out.address).starts_with("aaa"));

        // check fails on wrong chars
        let args = mk_args(&["--starts-with", "0xerr"]);
        let create2_out = args.run();
        assert!(create2_out.is_err());

        // check fails on wrong x prefixed string provided
        let args = mk_args(&["--starts-with", "x00"]);
        let create2_out = args.run();
        assert!(create2_out.is_err());
    }

    #[test]
    fn matches_pattern() {
        let args = Create2Args::parse_from([
            "foundry-cli",
            "--init-code-hash=0x0000000000000000000000000000000000000000000000000000000000000000",
            "--matching=0xbbXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX",
        ]);
        let create2_out = args.run().unwrap();
        let address = create2_out.address;
        assert!(format!("{address:x}").starts_with("bb"));
    }

    #[test]
    fn create2_salt() {
        let args = Create2Args::parse_from([
            "foundry-cli",
            "--deployer=0x8ba1f109551bD432803012645Ac136ddd64DBA72",
            "--salt=0x7c5ea36004851c764c44143b1dcb59679b11c9a68e5f41497f6cf3d480715331",
            "--init-code=0x6394198df16000526103ff60206004601c335afa6040516060f3",
        ]);
        let create2_out = args.run().unwrap();
        let address = create2_out.address;
        assert_eq!(address, address!("0x533AE9D683B10C02EBDB05471642F85230071FC3"));
    }

    #[test]
    fn create2_init_code() {
        let init_code = "00";
        let args =
            Create2Args::parse_from(["foundry-cli", "--starts-with=cc", "--init-code", init_code]);
        let create2_out = args.run().unwrap();
        let address = create2_out.address;
        assert!(format!("{address:x}").starts_with("cc"));
        let salt = create2_out.salt;
        let deployer = Address::from_str(DEPLOYER).unwrap();
        assert_eq!(address, deployer.create2_from_code(salt, hex::decode(init_code).unwrap()));
    }

    #[test]
    fn create2_init_code_hash() {
        let init_code_hash = "bc36789e7a1e281436464229828f817d6612f7b477d66591ff96a9e064bcc98a";
        let args = Create2Args::parse_from([
            "foundry-cli",
            "--starts-with=dd",
            "--init-code-hash",
            init_code_hash,
        ]);
        let create2_out = args.run().unwrap();
        let address = create2_out.address;
        assert!(format!("{address:x}").starts_with("dd"));

        let salt = create2_out.salt;
        let deployer = Address::from_str(DEPLOYER).unwrap();

        assert_eq!(
            address,
            deployer
                .create2(salt, B256::from_slice(hex::decode(init_code_hash).unwrap().as_slice()))
        );
    }

    #[test]
    fn create2_caller() {
        let init_code_hash = "bc36789e7a1e281436464229828f817d6612f7b477d66591ff96a9e064bcc98a";
        let args = Create2Args::parse_from([
            "foundry-cli",
            "--starts-with=dd",
            "--init-code-hash",
            init_code_hash,
            "--caller=0x66f9664f97F2b50F62D13eA064982f936dE76657",
        ]);
        let create2_out = args.run().unwrap();
        let address = create2_out.address;
        let salt = create2_out.salt;
        assert!(format!("{address:x}").starts_with("dd"));
        assert!(format!("{salt:x}").starts_with("66f9664f97f2b50f62d13ea064982f936de76657"));
    }

    #[test]
    fn deterministic_seed() {
        let args = Create2Args::parse_from([
            "foundry-cli",
            "--starts-with=0x00",
            "--init-code-hash=0x479d7e8f31234e208d704ba1a123c76385cea8a6981fd675b784fbd9cffb918d",
            "--seed=0x479d7e8f31234e208d704ba1a123c76385cea8a6981fd675b784fbd9cffb918d",
            "-j1",
        ]);
        let out = args.run().unwrap();
        assert_eq!(out.address, address!("0x00614b3D65ac4a09A376a264fE1aE5E5E12A6C43"));
        assert_eq!(
            out.salt,
            b256!("0x322113f523203e2c0eb00bbc8e69208b0eb0c8dad0eaac7b01d64ff016edb40d"),
        );
    }

    #[test]
    fn deterministic_output() {
        let args = Create2Args::parse_from([
            "foundry-cli",
            "--starts-with=0x00",
            "--init-code-hash=0x479d7e8f31234e208d704ba1a123c76385cea8a6981fd675b784fbd9cffb918d",
            "--no-random",
            "-j1",
        ]);
        let out = args.run().unwrap();
        assert_eq!(out.address, address!("0x00bF495b8b42fdFeb91c8bCEB42CA4eE7186AEd2"));
        assert_eq!(
            out.salt,
            b256!("0x000000000000000000000000000000000000000000000000df00000000000000"),
        );
    }

    #[test]
    fn j0() {
        let args = Create2Args::try_parse_from([
            "foundry-cli",
            "--starts-with=00",
            "--init-code-hash",
            &B256::ZERO.to_string(),
            "-j0",
        ])
        .unwrap();
        assert_eq!(args.threads, Some(0));
    }
}
