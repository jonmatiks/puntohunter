//! The one parser for Stratum usernames, `<address>.<worker>[#<secret>][+<option>]...`: mining.authorize and
//! the V2 channel open use it, and the portal lookup reuses it. The
//! options after the first `+` are a worker's choices: a finder part, and `name`.
//! The secret after a `#` makes its fleet private; over V1 the password may carry it instead.

use std::fmt;

use bitcoin::{Address, address::NetworkUnchecked};

use crate::config::{self, Network, Portion, RawPortion};

const MAX_WORKER_LEN: usize = 64;
/// The fewest characters a fleet secret takes.
pub const MIN_SECRET_LEN: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Username {
    /// The payout address, checked against the configured network.
    pub address: Address,
    /// Everything after the first dot, up to the secret or the options; empty when the username is the
    /// address alone.
    pub worker: String,
    /// The fleet secret, after a `#` or, over V1, in the password; None for a device in its address's open
    /// fleet.
    pub secret: Option<Secret>,
    /// The choices after the first `+`, which identity leaves out.
    pub options: Options,
}

/// A fleet secret. It enters the handles and is kept nowhere else, so its debug form never shows
/// it, and a username is logged through [`redacted`].
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// The secret a V1 password carries, once its comma-separated `d=N` hints are taken out: none
    /// when what is left is empty, `x`, or shorter than [`MIN_SECRET_LEN`], as most firmware ships it.
    pub fn from_password(password: &str) -> Option<Secret> {
        let rest: Vec<&str> = password
            .split(',')
            .filter(|part| !part.trim().starts_with("d="))
            .collect();
        let rest = rest.join(",");
        let rest = rest.trim();
        (rest != "x" && rest.chars().count() >= MIN_SECRET_LEN).then(|| Secret(rest.to_owned()))
    }

    /// The secret itself, for the handles alone.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// A worker's choices, each optional.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// The finder part it asked for, as written; None to take its pool's `finder`.
    pub part: Option<Portion>,
    /// Whether it asked for its name in the coinbase with `+name`.
    pub name: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UsernameError {
    #[error("not a Bitcoin address")]
    Address,
    #[error("not an address on {0}")]
    Network(&'static str),
    #[error("worker names take at most {MAX_WORKER_LEN} letters, digits, '.', '-' and '_'")]
    Worker,
    #[error(
        "an empty option: each '+' takes a part, such as 50, 12.5, 0.5btc, or 50000sats, or name"
    )]
    EmptyOption,
    #[error("option {0:?} is neither a part, such as 50, 12.5, 0.5btc, or 50000sats, nor name")]
    Option(String),
    #[error("part {0:?}: {1}")]
    Part(String, String),
    #[error("{0} is given twice")]
    Twice(&'static str),
    #[error("a fleet password after '#' takes at least {MIN_SECRET_LEN} characters, and no '+'")]
    ShortSecret,
    #[error("a fleet password is given both after '#' and as the password: give one")]
    TwoSecrets,
}

pub fn parse(username: &str, network: Network) -> Result<Username, UsernameError> {
    // Neither an address nor a worker name holds '+', and a part such as 12.5 holds a dot, so the options are
    // cut off before the address and worker are split. The secret, before them, holds no '+' either.
    let (username, options) = match username.split_once('+') {
        Some((username, suffix)) => (username, options(suffix)?),
        None => (username, Options::default()),
    };
    // Neither an address nor a worker name holds '#', so the secret starts at the first.
    let (username, secret) = match username.split_once('#') {
        Some((_, secret)) if secret.chars().count() < MIN_SECRET_LEN => {
            return Err(UsernameError::ShortSecret);
        }
        Some((username, secret)) => (username, Some(Secret(secret.to_owned()))),
        None => (username, None),
    };
    let (address, worker) = username.split_once('.').unwrap_or((username, ""));
    let address: Address<NetworkUnchecked> = address.parse().map_err(|_| UsernameError::Address)?;
    let address = address
        .require_network(network.bitcoin())
        .map_err(|_| UsernameError::Network(network.chain()))?;
    let valid = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_');
    if worker.len() > MAX_WORKER_LEN || !worker.chars().all(valid) {
        return Err(UsernameError::Worker);
    }
    Ok(Username {
        address,
        worker: worker.to_owned(),
        secret,
        options,
    })
}

impl Username {
    /// The username with the secret a V1 `password` carries, if it carries one; a username that
    /// already has one after a `#` cannot take a second.
    pub fn with_password(mut self, password: &str) -> Result<Username, UsernameError> {
        match (&self.secret, Secret::from_password(password)) {
            (Some(_), Some(_)) => Err(UsernameError::TwoSecrets),
            (None, Some(secret)) => {
                self.secret = Some(secret);
                Ok(self)
            }
            (_, None) => Ok(self),
        }
    }
}

/// `username` as sent, with any secret after a `#` blanked out, for a log line.
pub fn redacted(username: &str) -> String {
    match username.split_once('#') {
        Some((identity, rest)) => {
            let options = rest.find('+').map_or("", |i| &rest[i..]);
            format!("{identity}#…{options}")
        }
        None => username.to_owned(),
    }
}

/// The options of a suffix, the text after the first `+`: each one after a `+`, in any order, each at most once.
fn options(suffix: &str) -> Result<Options, UsernameError> {
    let mut options = Options::default();
    for option in suffix.split('+') {
        match option {
            "" => return Err(UsernameError::EmptyOption),
            "name" if options.name => return Err(UsernameError::Twice("name")),
            "name" => options.name = true,
            _ => {
                let part = part(option)?;
                if options.part.replace(part).is_some() {
                    return Err(UsernameError::Twice("a part"));
                }
            }
        }
    }
    Ok(options)
}

/// A part as the config takes a portion: a percent, such as `50` or `12.5`; `btc`, such as `0.5btc`; or whole
/// `sats`, such as `50000sats`, each held to `config::portion`'s precision and range.
fn part(option: &str) -> Result<Portion, UsernameError> {
    let (number, unit) = match option.find(|c: char| !c.is_ascii_digit() && c != '.') {
        Some(i) => option.split_at(i),
        None => (option, ""),
    };
    // Digits with at most one inner dot: no sign, exponent, or bare dot, which a float parser would take.
    let (whole, fraction) = number.split_once('.').unwrap_or((number, "0"));
    let decimal = |digits: &str| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
    if !(decimal(whole) && decimal(fraction)) || !matches!(unit, "" | "btc" | "sats") {
        return Err(UsernameError::Option(option.to_owned()));
    }
    let value: f64 = number
        .parse()
        .map_err(|_| UsernameError::Option(option.to_owned()))?;
    let raw = match unit {
        "" => RawPortion {
            percent: Some(value),
            ..RawPortion::default()
        },
        "btc" => RawPortion {
            btc: Some(value),
            ..RawPortion::default()
        },
        _ => RawPortion {
            sats: Some(value),
            ..RawPortion::default()
        },
    };
    config::portion(raw, true).map_err(|reason| UsernameError::Part(option.to_owned(), reason))
}

#[cfg(test)]
mod tests {
    use bitcoin::Amount;

    use super::*;
    use crate::config::Config;

    /// The pool wallets of the checked-in configs, standing in for miner addresses.
    fn address(network: &str) -> String {
        let path = format!("{}/config/{network}.toml", env!("CARGO_MANIFEST_DIR"));
        let config: Config = std::fs::read_to_string(path).unwrap().parse().unwrap();
        config.addresses.pool_wallet.to_string()
    }

    #[test]
    fn splits_address_and_worker() {
        let regtest = address("regtest");
        let parsed = parse(&format!("{regtest}.bitaxe"), Network::Regtest).unwrap();
        assert_eq!(parsed.address.to_string(), regtest);
        assert_eq!(parsed.worker, "bitaxe");

        let dotted = parse(&format!("{regtest}.rack.1"), Network::Regtest).unwrap();
        assert_eq!(dotted.worker, "rack.1");
        assert_eq!(parse(&regtest, Network::Regtest).unwrap().worker, "");
    }

    #[test]
    fn reads_the_options_after_the_first_plus_in_any_order() {
        let regtest = address("regtest");
        let options = |suffix: &str| parse(&format!("{regtest}{suffix}"), Network::Regtest);

        let plain = options(".rig").unwrap();
        assert_eq!(plain.options, Options::default());
        for (suffix, part, name) in [
            (".rig+50", Portion::Ppm(500_000), false),
            (".rig+12.5", Portion::Ppm(125_000), false),
            (".rig+12.3456", Portion::Ppm(123_456), false),
            (".rig+0", Portion::Ppm(0), false),
            (".rig+100", Portion::Ppm(1_000_000), false),
            (
                ".rig+0.5btc",
                Portion::Amount(Amount::from_sat(50_000_000)),
                false,
            ),
            (
                ".rig+50000000sats",
                Portion::Amount(Amount::from_sat(50_000_000)),
                false,
            ),
            (".rig+name+33.33", Portion::Ppm(333_300), true),
            (".rig+33.33+name", Portion::Ppm(333_300), true),
        ] {
            let parsed = options(suffix).unwrap();
            assert_eq!(parsed.worker, "rig", "{suffix}");
            assert_eq!(
                parsed.options,
                Options {
                    part: Some(part),
                    name
                },
                "{suffix}"
            );
        }
        assert_eq!(
            options(".rig+name").unwrap().options,
            Options {
                part: None,
                name: true
            }
        );

        // The dot of a part is not the worker's, and an address alone takes options too.
        assert_eq!(options(".rack.1+12.5").unwrap().worker, "rack.1");
        let bare = options("+12.5+name").unwrap();
        assert_eq!((bare.worker.as_str(), bare.options.name), ("", true));
    }

    #[test]
    fn refuses_a_malformed_unknown_or_repeated_option() {
        let regtest = address("regtest");
        let parsed = |suffix: &str| parse(&format!("{regtest}.rig{suffix}"), Network::Regtest);
        for suffix in ["+", "++name", "+name+", "+50++name"] {
            assert_eq!(parsed(suffix), Err(UsernameError::EmptyOption), "{suffix}");
        }
        for suffix in [
            "+Name", "+names", "+50%", "+-5", "+1e2", "+.5", "+5.", "+1.2.3", "+inf", "+0.5BTC",
            "+5 sats", "+5sat", "+0x10",
        ] {
            assert!(
                matches!(parsed(suffix), Err(UsernameError::Option(_))),
                "{suffix}: {:?}",
                parsed(suffix)
            );
        }
        for suffix in [
            "+100.0001",
            "+101",
            "+12.34567",
            "+0.000000001btc",
            "+21000001btc",
            "+2100000000000001sats",
        ] {
            assert!(
                matches!(parsed(suffix), Err(UsernameError::Part(..))),
                "{suffix}: {:?}",
                parsed(suffix)
            );
        }
        assert_eq!(parsed("+50+0.5btc"), Err(UsernameError::Twice("a part")));
        assert_eq!(parsed("+name+50+name"), Err(UsernameError::Twice("name")));
        // A '+' in the worker's place is the suffix's, never a worker name.
        assert_eq!(parsed("+rig2"), Err(UsernameError::Option("rig2".into())));
    }

    #[test]
    fn reads_a_secret_after_the_first_hash_before_the_options() {
        let regtest = address("regtest");
        let parsed = |text: &str| parse(&format!("{regtest}{text}"), Network::Regtest);
        let secret = |text: &str| parsed(text).unwrap().secret.map(|s| s.expose().to_owned());

        assert_eq!(secret(".rig"), None);
        assert_eq!(
            secret(".rig#S3cret!#x"),
            Some("S3cret!#x".into()),
            "any character but '+'"
        );
        let full = parsed(".rig#12345678+50+name").unwrap();
        assert_eq!(full.worker, "rig");
        assert_eq!(full.secret.unwrap().expose(), "12345678");
        assert_eq!(full.options.part, Some(Portion::Ppm(500_000)));
        assert_eq!(
            secret("#12345678"),
            Some("12345678".into()),
            "an address alone too"
        );
        assert_eq!(parsed(".rig#1234567"), Err(UsernameError::ShortSecret));
        assert_eq!(parsed(".rig#"), Err(UsernameError::ShortSecret));
        assert_eq!(
            parsed(".rig#1234+abcd"),
            Err(UsernameError::Option("abcd".into())),
            "a '+' starts the options"
        );
        assert!(
            !format!("{:?}", parsed(".rig#S3cret-word").unwrap()).contains("S3cret"),
            "the debug form hides it"
        );
    }

    #[test]
    fn a_v1_password_carries_a_secret_without_its_hints() {
        let secret = |password: &str| Secret::from_password(password).map(|s| s.0);
        for none in ["", "x", "1234567", "d=512", "x,d=512", " , d=0.5"] {
            assert_eq!(secret(none), None, "{none:?}");
        }
        assert_eq!(secret("12345678"), Some("12345678".into()));
        assert_eq!(secret("garage-pass,d=512"), Some("garage-pass".into()));
        assert_eq!(secret("d=512, my+fleet+pass"), Some("my+fleet+pass".into()));

        let regtest = address("regtest");
        let plain = parse(&format!("{regtest}.rig"), Network::Regtest).unwrap();
        let hashed = parse(&format!("{regtest}.rig#12345678"), Network::Regtest).unwrap();
        let with = plain.clone().with_password("garage-pass").unwrap();
        assert_eq!(with.secret.unwrap().expose(), "garage-pass");
        assert_eq!(plain.clone().with_password("x").unwrap(), plain);
        assert_eq!(hashed.clone().with_password("x,d=512").unwrap(), hashed);
        assert_eq!(
            hashed.with_password("garage-pass"),
            Err(UsernameError::TwoSecrets)
        );
    }

    #[test]
    fn a_logged_username_hides_its_secret() {
        assert_eq!(redacted("bc1q.rig"), "bc1q.rig");
        assert_eq!(redacted("bc1q.rig#S3cret!+50+name"), "bc1q.rig#…+50+name");
        assert_eq!(redacted("bc1q.rig#S3cret!"), "bc1q.rig#…");
        assert_eq!(redacted("bc1q.rig+50"), "bc1q.rig+50");
    }

    #[test]
    fn refuses_an_address_for_the_other_network() {
        let mainnet = address("mainnet");
        assert_eq!(
            parse(&format!("{mainnet}.bitaxe"), Network::Regtest),
            Err(UsernameError::Network("regtest"))
        );
        assert!(parse(&format!("{mainnet}.bitaxe"), Network::Main).is_ok());
    }

    #[test]
    fn refuses_what_is_not_an_address_or_a_worker() {
        assert_eq!(parse("worker1", Network::Main), Err(UsernameError::Address));
        let mainnet = address("mainnet");
        assert_eq!(
            parse(&format!("{mainnet}.a b"), Network::Main),
            Err(UsernameError::Worker)
        );
        let long = "w".repeat(MAX_WORKER_LEN + 1);
        assert_eq!(
            parse(&format!("{mainnet}.{long}"), Network::Main),
            Err(UsernameError::Worker)
        );
    }
}
