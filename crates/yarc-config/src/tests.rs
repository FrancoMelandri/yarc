use std::time::Duration;

use super::*;

const SAMPLE: &str = r#"
services:
  db:
    image: postgres:16
    env: { POSTGRES_PASSWORD: secret }
    volumes: [ "pgdata:/var/lib/postgresql/data" ]
    health: { cmd: "pg_isready", interval: 5s }
  api:
    image: myorg/api:1.2
    ports: [ "8080:80" ]
    needs: { db: healthy }
"#;

fn err(yaml: &str) -> ConfigError {
    parse(yaml).unwrap_err()
}

#[test]
fn parses_sample_project() {
    let p = parse(SAMPLE).unwrap();
    let db = &p.services["db"];
    assert_eq!(db.image, "postgres:16");
    assert_eq!(db.env["POSTGRES_PASSWORD"], "secret");
    assert_eq!(db.health.as_ref().unwrap().interval, Duration::from_secs(5));
    assert_eq!(
        db.volumes[0],
        VolumeMount {
            source: VolumeSource::Named("pgdata".into()),
            target: "/var/lib/postgresql/data".into(),
            read_only: false
        }
    );
    let api = &p.services["api"];
    assert_eq!(
        api.ports[0],
        PortMapping { host: 8080, container: 80, protocol: Protocol::Tcp }
    );
    assert_eq!(api.needs["db"], Condition::Healthy);
}

#[test]
fn health_interval_defaults() {
    let p = parse("services:\n  a:\n    image: x\n    health: { cmd: ok }\n").unwrap();
    assert_eq!(
        p.services["a"].health.as_ref().unwrap().interval,
        DEFAULT_HEALTH_INTERVAL
    );
}

#[test]
fn port_protocol_and_bounds() {
    let p = parse("services:\n  a:\n    image: x\n    ports: ['53:53/udp']\n").unwrap();
    assert_eq!(p.services["a"].ports[0].protocol, Protocol::Udp);
    for bad in ["80", "0:80", "70000:80", "a:b", "80:80/sctp"] {
        let y = format!("services:\n  a:\n    image: x\n    ports: ['{bad}']\n");
        assert!(matches!(err(&y), ConfigError::InvalidPort { .. }), "{bad}");
    }
}

#[test]
fn volume_kinds() {
    let y = "services:\n  a:\n    image: x\n    volumes: ['./src:/app:ro', '/etc/h:/h', 'data:/d']\n";
    let v = &parse(y).unwrap().services["a"].volumes;
    assert_eq!(v[0].source, VolumeSource::Host("./src".into()));
    assert!(v[0].read_only);
    assert_eq!(v[1].source, VolumeSource::Host("/etc/h".into()));
    assert_eq!(v[2].source, VolumeSource::Named("data".into()));
    for bad in ["justone", "a:rel/path", "a:/b:rw", ":/b"] {
        let y = format!("services:\n  a:\n    image: x\n    volumes: ['{bad}']\n");
        assert!(matches!(err(&y), ConfigError::InvalidVolume { .. }), "{bad}");
    }
}

#[test]
fn duration_units() {
    for (s, ms) in [("500ms", 500u64), ("2s", 2000), ("1m", 60_000), ("1h", 3_600_000)] {
        let y = format!(
            "services:\n  a:\n    image: x\n    health: {{ cmd: ok, interval: {s} }}\n"
        );
        let p = parse(&y).unwrap();
        assert_eq!(
            p.services["a"].health.as_ref().unwrap().interval,
            Duration::from_millis(ms)
        );
    }
    let y = "services:\n  a:\n    image: x\n    health: { cmd: ok, interval: soon }\n";
    assert!(matches!(err(y), ConfigError::InvalidDuration { .. }));
}

#[test]
fn rejects_empty_image_and_no_services() {
    assert!(matches!(
        err("services:\n  a:\n    image: ''\n"),
        ConfigError::EmptyImage(_)
    ));
    assert_eq!(err("services: {}\n"), ConfigError::NoServices);
    assert_eq!(err("{}\n"), ConfigError::NoServices);
}

#[test]
fn rejects_unknown_keys_and_bad_yaml() {
    assert!(matches!(
        err("services:\n  a:\n    image: x\n    depends_on: [b]\n"),
        ConfigError::Yaml(_)
    ));
    assert!(matches!(err("services: ["), ConfigError::Yaml(_)));
}

#[test]
fn needs_validation() {
    let unknown = "services:\n  a:\n    image: x\n    needs: { b: started }\n";
    assert!(matches!(err(unknown), ConfigError::UnknownNeed { .. }));

    let selfn = "services:\n  a:\n    image: x\n    needs: { a: started }\n";
    assert_eq!(err(selfn), ConfigError::SelfNeed("a".into()));

    let nohealth =
        "services:\n  a:\n    image: x\n    needs: { b: healthy }\n  b:\n    image: y\n";
    assert!(matches!(
        err(nohealth),
        ConfigError::NeedsHealthWithoutCheck { .. }
    ));
}

#[test]
fn detects_cycles() {
    let y = "services:\n  a:\n    image: x\n    needs: { b: started }\n  b:\n    image: y\n    needs: { c: started }\n  c:\n    image: z\n    needs: { a: started }\n";
    assert!(matches!(err(y), ConfigError::Cycle(_)));
}

#[test]
fn cmd_is_a_list_of_arguments_and_optional() {
    let p = parse("services:\n  a:\n    image: x\n    cmd: [sh, -c, 'echo hi; sleep 1']\n  b:\n    image: y\n").unwrap();
    assert_eq!(p.services["a"].cmd, ["sh", "-c", "echo hi; sleep 1"]);
    assert!(p.services["b"].cmd.is_empty());
}

#[test]
fn cmd_must_be_a_list_not_a_string() {
    let err = parse("services:\n  a:\n    image: x\n    cmd: 'echo hi'\n").unwrap_err();
    assert!(matches!(err, ConfigError::Yaml(_)), "{err}");
}
