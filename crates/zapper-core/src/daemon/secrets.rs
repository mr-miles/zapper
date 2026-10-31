//! Whether a secret's value may go where it is wanted: the one place that
//! decides it, for the proxy's header injection, AWS signing on the host,
//! and the database relay. Which secret a name means is decided here too.

use std::collections::BTreeMap;

use zapper_policy::{Config, Secret, SecretMode};

/// What a secret's value is wanted for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Use {
    /// Put into an inspected request's headers.
    Header,
    /// Signing an AWS request on the host.
    Sign,
    /// Authenticating to a database the relay connects to.
    Database,
}

/// The secret `name` means for a session: the config's, else the session's
/// own (`Session::put_secret` refuses a name the config has).
pub(crate) fn find<'a>(
    config: &'a Config,
    own: &'a BTreeMap<String, Secret>,
    name: &str,
) -> Option<&'a Secret> {
    config.secrets.get(name).or_else(|| own.get(name))
}

/// Whether `secret` (`name`) may be used for `what` with `host`, over a
/// connection that is TLS or not; why not if it may not. It must be of the
/// mode the use is for, `host` among its `hosts`, and the connection TLS
/// unless it is `plaintext`. A database's connection is decided for each
/// leg as the relay makes it (`relay::server_leg`), so not here.
pub(crate) fn check(
    name: &str,
    secret: &Secret,
    what: Use,
    host: &str,
    tls: bool,
) -> Result<(), String> {
    let mode_fits = match what {
        Use::Header => secret.mode != SecretMode::Aws,
        Use::Sign => secret.mode == SecretMode::Aws,
        Use::Database => secret.mode == SecretMode::DbAuth,
    };
    if !mode_fits {
        return Err(format!(
            "secret {name} is a {:?} secret, not one for {what:?}",
            secret.mode
        ));
    }
    if !secret.allows_host(host) {
        return Err(match what {
            Use::Header => format!("secret {name} may not be sent to {host}"),
            Use::Sign => format!("secret {name} may not sign for {host}"),
            Use::Database => {
                format!("secret {name} isn't for {host}: it isn't among the secret's hosts")
            }
        });
    }
    if !tls && !secret.plaintext && what != Use::Database {
        let verb = if what == Use::Sign { "sign" } else { "be sent" };
        return Err(format!(
            "secret {name} may not {verb} over plain HTTP (set plaintext: true on it to allow)"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(mode: &str, plaintext: bool) -> Secret {
        serde_yaml::from_str(&format!(
            "{{ value: v, mode: {mode}, hosts: [api.example], plaintext: {plaintext}, user: u }}"
        ))
        .unwrap()
    }

    /// Each use, against each mode, host and connection: allowed only
    /// where all three fit.
    #[test]
    fn a_secret_goes_only_where_its_mode_hosts_and_connection_fit() {
        let modes = ["proxy", "guest", "dbauth", "aws"];
        for what in [Use::Header, Use::Sign, Use::Database] {
            for mode in modes {
                for plaintext in [false, true] {
                    let s = secret(mode, plaintext);
                    for host in ["api.example", "evil.example"] {
                        for tls in [true, false] {
                            let mode_fits = match what {
                                Use::Header => mode != "aws",
                                Use::Sign => mode == "aws",
                                Use::Database => mode == "dbauth",
                            };
                            let conn_fits = tls || plaintext || what == Use::Database;
                            let want = mode_fits && host == "api.example" && conn_fits;
                            let got = check("s", &s, what, host, tls);
                            assert_eq!(
                                got.is_ok(),
                                want,
                                "{what:?} {mode} plaintext={plaintext} {host} tls={tls}: {got:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// The config's secret wins over a session's own of the same name.
    #[test]
    fn the_configs_secret_is_found_first() {
        let mut config = Config::default();
        config.secrets.insert("s".into(), secret("proxy", false));
        let own: BTreeMap<String, Secret> = [("s".to_string(), secret("aws", false))].into();
        assert_eq!(find(&config, &own, "s").unwrap().mode, SecretMode::Proxy);
        let own: BTreeMap<String, Secret> = [("t".to_string(), secret("aws", false))].into();
        assert_eq!(find(&config, &own, "t").unwrap().mode, SecretMode::Aws);
        assert!(find(&config, &own, "u").is_none());
    }
}
