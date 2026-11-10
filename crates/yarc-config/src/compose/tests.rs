use std::time::Duration;

use super::*;
use crate::{Protocol, VolumeMount};

fn env(n: &str) -> Option<String> {
    match n {
        "HOST" => Some("10.0.0.5".into()),
        "PORT" => Some("9000".into()),
        "EMPTY" => Some(String::new()),
        _ => None,
    }
}

fn parse(y: &str) -> Result<Project, ConfigError> {
    parse_compose(y, &env)
}

fn ok(y: &str) -> Project {
    parse(y).unwrap_or_else(|e| panic!("{e}"))
}

fn err(y: &str) -> String {
    parse(y).unwrap_err().to_string()
}

/// One service `a` with the given extra lines (already indented by 4).
fn svc(extra: &str) -> String {
    format!("services:\n  a:\n    image: img:1\n{extra}\n")
}

#[test]
fn minimal_service() {
    let p = ok("services:\n  a:\n    image: alpine:3\n");
    let a = &p.services["a"];
    assert_eq!(a.image, "alpine:3");
    assert!(a.cmd.is_empty() && a.env.is_empty() && a.ports.is_empty() && a.volumes.is_empty());
    assert_eq!((&a.entrypoint, &a.working_dir, &a.container_name, &a.user), (&None, &None, &None, &None));
}

#[test]
fn command_string_list_and_entrypoint() {
    let p = ok(&svc("    command: sh -c \"echo 'a b' && sleep 1\"\n    entrypoint: [/bin/tini, --]"));
    let a = &p.services["a"];
    assert_eq!(a.cmd, ["sh", "-c", "echo 'a b' && sleep 1"]);
    assert_eq!(a.entrypoint.as_deref(), Some(&["/bin/tini".to_string(), "--".to_string()][..]));

    let p = ok(&svc("    command: [sleep, 10]"));
    assert_eq!(p.services["a"].cmd, ["sleep", "10"]);
    // an empty entrypoint clears the image's
    let p = ok(&svc("    entrypoint: []"));
    assert_eq!(p.services["a"].entrypoint, Some(vec![]));
    assert!(err(&svc("    command: \"echo 'open\"")).contains("unbalanced quotes"));
}

#[test]
fn split_words_rules() {
    assert_eq!(split_words("  a   b ").unwrap(), ["a", "b"]);
    assert_eq!(split_words("a\\ b c").unwrap(), ["a b", "c"]);
    assert_eq!(split_words("'' x").unwrap(), ["", "x"]);
    assert_eq!(split_words("\"a\\\"b\" 'c\\d'").unwrap(), ["a\"b", "c\\d"]);
    assert_eq!(split_words("").unwrap(), Vec::<String>::new());
    assert!(split_words("\"open").is_none());
}

#[test]
fn environment_list_map_and_passthrough() {
    let p = ok(&svc("    environment:\n      - A=1\n      - B=x=y\n      - C=\n      - HOST\n      - UNSET_VAR"));
    let e = &p.services["a"].env;
    assert_eq!((e["A"].as_str(), e["B"].as_str(), e["C"].as_str(), e["HOST"].as_str()), ("1", "x=y", "", "10.0.0.5"));
    assert!(!e.contains_key("UNSET_VAR"));

    let p = ok(&svc("    environment:\n      A: 1\n      B: true\n      C: text\n      PORT:\n      NOPE:"));
    let e = &p.services["a"].env;
    assert_eq!((e["A"].as_str(), e["B"].as_str(), e["C"].as_str(), e["PORT"].as_str()), ("1", "true", "text", "9000"));
    assert!(!e.contains_key("NOPE"));
}

#[test]
fn interpolation_in_values_not_comments_or_keys() {
    let y = svc("    environment:\n      - ADDR=${HOST:?need HOST}:${PORT:-1}\n      - D=${NOPE:-fallback}\n      - LIT=$$HOME  # ${REQUIRED:?never read}");
    let e = &ok(&y).services["a"].env;
    assert_eq!(e["ADDR"], "10.0.0.5:9000");
    assert_eq!(e["D"], "fallback");
    assert_eq!(e["LIT"], "$HOME");
}

#[test]
fn interpolation_errors_name_the_place_and_never_the_value() {
    let e = err(&svc("    environment:\n      - A=${MISSING:?set MISSING in .env}"));
    assert_eq!(e, "services.a.environment[0]: required variable MISSING is missing a value: set MISSING in .env");
    let e = err("services:\n  a:\n    image: \"bad$\"\n");
    assert!(e.starts_with("services.a.image: invalid interpolation"), "{e}");
}

#[test]
fn ports_short_syntax() {
    let p = ok(&svc("    ports:\n      - \"8080:80\"\n      - \"127.0.0.1:${PORT:-18888}:18888\"\n      - localhost:5000:5000/udp\n      - \"53:53/tcp\""));
    let ports = &p.services["a"].ports;
    assert_eq!((ports[0].host, ports[0].container, ports[0].protocol), (8080, 80, Protocol::Tcp));
    assert_eq!((ports[1].host, ports[1].container), (9000, 18888));
    assert_eq!(ports[2].protocol, Protocol::Udp);
    assert_eq!(ports[3].protocol, Protocol::Tcp);
}

#[test]
fn ports_unsupported_forms() {
    for (line, needle) in [
        ("- \"80\"", "random host port"),
        ("- 80", "random host port"),
        ("- \"0.0.0.0:80:80\"", "127.0.0.1 only"),
        ("- \"[::1]:80:80\"", "expected"),
        ("- \"8000-8010:80\"", "1-65535"),
        ("- \"127.0.0.1::80\"", "1-65535"),
        ("- {target: 80, published: 80}", "long syntax"),
    ] {
        let e = err(&svc(&format!("    ports:\n      {line}")));
        assert!(e.contains(needle), "{line}: {e}");
    }
}

#[test]
fn volumes_named_host_and_modes() {
    let y = "services:\n  a:\n    image: i\n    volumes:\n      - data:/data\n      - ./certs:/certs:ro\n      - /abs:/abs:rw\nvolumes:\n  data:\n";
    let v = &ok(y).services["a"].volumes;
    assert_eq!(
        v[0],
        VolumeMount { source: VolumeSource::Named("data".into()), target: "/data".into(), read_only: false }
    );
    assert_eq!(
        v[1],
        VolumeMount { source: VolumeSource::Host("./certs".into()), target: "/certs".into(), read_only: true }
    );
    assert!(!v[2].read_only);
}

#[test]
fn volumes_must_be_declared_and_short() {
    let e = err(&svc("    volumes:\n      - nope:/d"));
    assert!(e.contains("'nope' is not declared"), "{e}");
    let e = err(&svc("    volumes:\n      - {type: bind, source: ., target: /x}"));
    assert!(e.contains("short"), "{e}");
    let e = err("services:\n  a:\n    image: i\nvolumes:\n  data:\n    driver: local\n");
    assert!(e.contains("volumes.data") && e.contains("not supported"), "{e}");
    // `data: {}` and `data: ` are both plain declarations
    ok("services:\n  a:\n    image: i\n    volumes: [\"d:/d\", \"e:/e\"]\nvolumes:\n  d: {}\n  e:\n");
}

#[test]
fn depends_on_list_and_map() {
    let y = "services:\n  db:\n    image: i\n    healthcheck:\n      test: [CMD, pg_isready]\n  cache:\n    image: i\n  a:\n    image: i\n    depends_on: [cache]\n  b:\n    image: i\n    depends_on:\n      db:\n        condition: service_healthy\n      cache:\n        condition: service_started\n  c:\n    image: i\n    depends_on:\n      cache: {}\n";
    let p = ok(y);
    assert_eq!(p.services["a"].needs["cache"], Condition::Started);
    assert_eq!(p.services["b"].needs["db"], Condition::Healthy);
    assert_eq!(p.services["b"].needs["cache"], Condition::Started);
    assert_eq!(p.services["c"].needs["cache"], Condition::Started);
}

#[test]
fn depends_on_problems() {
    let e = err(&svc("    depends_on:\n      x:\n        condition: service_completed_successfully"));
    assert!(e.contains("service_completed_successfully"), "{e}");
    let e = err(&svc("    depends_on:\n      x:\n        required: false"));
    assert!(e.contains("'required'"), "{e}");
    assert_eq!(err(&svc("    depends_on: [ghost]")), "service 'a' needs unknown service 'ghost'");
    // healthy needs a check on the target (shared validation)
    let e = err("services:\n  d:\n    image: i\n  a:\n    image: i\n    depends_on:\n      d:\n        condition: service_healthy\n");
    assert!(e.contains("no health check"), "{e}");
}

#[test]
fn user_workdir_container_name() {
    let p = ok(&svc("    user: \"1000:1000\"\n    working_dir: /srv\n    container_name: my-app.1"));
    let a = &p.services["a"];
    assert_eq!(
        (a.user.as_deref(), a.working_dir.as_deref(), a.container_name.as_deref()),
        (Some("1000:1000"), Some("/srv"), Some("my-app.1"))
    );
    assert_eq!(ok(&svc("    user: 0")).services["a"].user.as_deref(), Some("0"));
    assert!(err(&svc("    working_dir: rel")).contains("absolute"));
    assert!(err(&svc("    container_name: \"bad name\"")).contains("container_name"));
    assert!(err(&svc("    container_name: \"\"")).contains("container_name"));
}

#[test]
fn container_name_collisions() {
    // equal to its own service name is the common case and fine
    ok("services:\n  a:\n    image: i\n    container_name: a\n");
    let e = err("services:\n  a:\n    image: i\n    container_name: b\n  b:\n    image: i\n");
    assert!(e.contains("collides"), "{e}");
    let e = err("services:\n  a:\n    image: i\n    container_name: z\n  b:\n    image: i\n    container_name: z\n");
    assert!(e.contains("collides"), "{e}");
}

#[test]
fn healthcheck_forms() {
    let h = |test: &str| {
        ok(&svc(&format!("    healthcheck:\n      test: {test}\n      interval: 1m30s"))).services["a"].health.clone()
    };
    let shell = h("[CMD-SHELL, \"pg_isready -U x\"]").unwrap();
    assert_eq!((shell.cmd.as_str(), shell.interval), ("pg_isready -U x", Duration::from_secs(90)));
    assert_eq!(h("\"curl -f http://x/\"").unwrap().cmd, "curl -f http://x/");
    assert_eq!(h("[CMD, curl, -f, \"http://x/a b\", \"it's\"]").unwrap().cmd, "curl -f 'http://x/a b' 'it'\\''s'");
    assert!(h("[NONE]").is_none());
    let d = ok(&svc("    healthcheck:\n      test: [CMD, true]")).services["a"].health.clone().unwrap();
    assert_eq!(d.interval, crate::model::DEFAULT_HEALTH_INTERVAL);
}

#[test]
fn healthcheck_problems() {
    for (hc, needle) in [
        ("test: [CMD, true]\n      retries: 3", "unsupported key 'retries'"),
        ("interval: 5s", "test is required"),
        ("test: [CMD]", "test must be"),
        ("test: [WRONG, x]", "test must be"),
        ("test: [CMD, true]\n      interval: soon", "invalid duration"),
    ] {
        let e = err(&svc(&format!("    healthcheck:\n      {hc}")));
        assert!(e.contains(needle), "{hc}: {e}");
    }
}

#[test]
fn unsupported_keys_are_errors_but_extensions_are_ignored() {
    for key in ["build: .", "networks: [n]", "cap_add: [NET_ADMIN]"] {
        let e = err(&svc(&format!("    {key}")));
        let name = key.split(':').next().unwrap();
        assert_eq!(e, format!("service 'a': unsupported key '{name}'"));
    }
    ok(&svc("    x-yarc: {user: root}\n    x-anything: 1"));
    // top level
    assert_eq!(err("services:\n  a:\n    image: i\nnetworks: {}\n"), "top level: unsupported key 'networks'");
    assert_eq!(err("services:\n  a:\n    image: i\nconfigs: {}\n"), "top level: unsupported key 'configs'");
    ok("version: \"3.9\"\nx-common: &c {image: i}\nservices:\n  a: *c\n");
}

#[test]
fn image_and_structure_errors() {
    assert!(err("services:\n  a:\n    command: x\n").contains("image: is required"));
    assert!(matches!(parse("services:\n  a:\n    image: ' '\n").unwrap_err(), ConfigError::EmptyImage(_)));
    assert_eq!(parse("services: {}\n").unwrap_err(), ConfigError::NoServices);
    assert!(matches!(parse("services: [").unwrap_err(), ConfigError::Yaml(_)));
}

#[test]
fn yaml_anchors_and_merge_keys_work() {
    let y = "x-base: &base\n  image: shared:1\n  environment: [K=v]\nservices:\n  a:\n    <<: *base\n  b:\n    <<: *base\n    image: other:2\n";
    let p = ok(y);
    assert_eq!(p.services["a"].image, "shared:1");
    assert_eq!((p.services["b"].image.as_str(), p.services["b"].env["K"].as_str()), ("other:2", "v"));
}

/// The shape of the real PMS bundles (without `restart`, `secrets`, `profiles` and
/// `stop_grace_period`, which arrive in C3/C4).
#[test]
fn pms_bundle_shape() {
    let y = r#"
services:
  pms:
    image: ssw-platform-pms:0.0.4
    container_name: pms
    environment:
      - PMS_FIAS_SERVER_IP_ADDRESS=${HOST:?set it in .env}
      - PMS_FIAS_SERVER_IP_PORT=${PMS_PORT:-5009}
      - OTEL_EXPORTER_OTLP_ENDPOINT=${PMS_OTEL_ENDPOINT:-}
      - JAVA_TOOL_OPTIONS=-XX:MaxRAMPercentage=75 -Dfile.encoding=UTF-8
    volumes:
      - pms-data:/data
    depends_on:
      - inspect
  inspect:
    image: ssw-platform-pms-inspect:0.0.3
    container_name: inspect
  dash:
    image: dotnet/aspire-dashboard:9.5
    ports:
      - "127.0.0.1:${ASPIRE_HOST_PORT:-18888}:18888"
volumes:
  pms-data:
"#;
    let p = ok(y);
    let pms = &p.services["pms"];
    assert_eq!(pms.env["PMS_FIAS_SERVER_IP_ADDRESS"], "10.0.0.5");
    assert_eq!(pms.env["PMS_FIAS_SERVER_IP_PORT"], "5009");
    assert_eq!(pms.env["OTEL_EXPORTER_OTLP_ENDPOINT"], "");
    assert_eq!(pms.env["JAVA_TOOL_OPTIONS"], "-XX:MaxRAMPercentage=75 -Dfile.encoding=UTF-8");
    assert_eq!(pms.needs["inspect"], Condition::Started);
    assert_eq!(p.services["dash"].ports[0].host, 18888);
}

const SECRETS: &str = "services:\n  a:\n    image: i\n    secrets: [tok, tok, key]\nsecrets:\n  tok:\n    file: ./secrets/tok.txt\n  key:\n    file: /abs/key\n";

#[test]
fn file_secrets_are_listed_once_with_their_files() {
    let a = &ok(SECRETS).services["a"];
    assert_eq!(
        a.secrets,
        [
            SecretMount { name: "tok".into(), file: "./secrets/tok.txt".into() },
            SecretMount { name: "key".into(), file: "/abs/key".into() },
        ]
    );
}

#[test]
fn secret_problems() {
    let e = err(&svc("    secrets: [nope]"));
    assert!(e.contains("'nope' is not declared"), "{e}");
    let e = err(&svc("    secrets:\n      - {source: s, target: t}"));
    assert!(e.contains("short form"), "{e}");
    for (decl, needle) in [
        ("s:\n    external: true", "unsupported key 'external'"),
        ("s:\n    environment: X", "unsupported key 'environment'"),
        ("s: {}", "only file secrets"),
        ("s:\n    file: ''", "must not be empty"),
        ("'a b':\n    file: x", "names may only"),
    ] {
        let e = err(&format!("services:\n  a:\n    image: i\nsecrets:\n  {decl}\n"));
        assert!(e.contains(needle), "{decl}: {e}");
    }
}

#[test]
fn profiles_and_stop_grace_period() {
    let a = &ok(&svc("    profiles: [observability, debug]\n    stop_grace_period: 1m30s")).services["a"];
    assert_eq!(a.profiles, ["observability", "debug"]);
    assert_eq!(a.stop_grace_period, Some(Duration::from_secs(90)));
    assert!(err(&svc("    stop_grace_period: soon")).contains("invalid duration"));
    assert!(err(&svc("    profiles: [\"bad name\"]")).contains("invalid profile name"));
}

fn profiled() -> Project {
    ok("services:\n  web:\n    image: i\n    depends_on: [db]\n  db:\n    image: i\n  dash:\n    image: i\n    profiles: [observability]\n  dbg:\n    image: i\n    profiles: [debug]\n")
}

fn names(p: &Project) -> Vec<&str> {
    p.services.keys().map(String::as_str).collect()
}

#[test]
fn inactive_profiles_are_dropped() {
    assert_eq!(names(&profiled().with_profiles(&[]).unwrap()), ["db", "web"]);
    assert_eq!(names(&profiled().with_profiles(&["observability".into()]).unwrap()), ["dash", "db", "web"]);
    let both = profiled().with_profiles(&["debug".into(), "observability".into()]).unwrap();
    assert_eq!(names(&both), ["dash", "db", "dbg", "web"]);
    assert_eq!(names(&profiled().with_profiles(&["*".into()]).unwrap()).len(), 4);
    assert_eq!(names(&profiled().with_profiles(&["unknown".into()]).unwrap()), ["db", "web"]);
}

#[test]
fn inactive_services_are_listed_with_the_profiles_that_enable_them() {
    let p = profiled();
    assert_eq!(
        p.inactive_services(&[]),
        [("dash".to_string(), vec!["observability".to_string()]), ("dbg".to_string(), vec!["debug".to_string()])]
    );
    assert_eq!(p.inactive_services(&["debug".into()]), [("dash".to_string(), vec!["observability".to_string()])]);
    assert!(p.inactive_services(&["*".into()]).is_empty());
    assert!(p.inactive_services(&["debug".into(), "observability".into()]).is_empty());
    // what it lists is exactly what with_profiles removes
    let kept = p.clone().with_profiles(&["debug".into()]).unwrap();
    assert_eq!(kept.services.len() + p.inactive_services(&["debug".into()]).len(), p.services.len());
}

#[test]
fn needing_a_service_in_an_inactive_profile_is_an_error() {
    let p = ok("services:\n  web:\n    image: i\n    depends_on: [dash]\n  dash:\n    image: i\n    profiles: [observability, extra]\n");
    let e = p.clone().with_profiles(&[]).unwrap_err().to_string();
    assert_eq!(e, "service 'web' needs 'dash', which is in the inactive profile(s) observability, extra; activate it with --profile");
    assert!(p.with_profiles(&["extra".into()]).is_ok());
    // nothing left at all
    let only = ok("services:\n  a:\n    image: i\n    profiles: [p]\n");
    assert_eq!(only.with_profiles(&[]).unwrap_err(), ConfigError::NoServices);
}

#[test]
fn restart_policies() {
    let policy = |v: &str| ok(&svc(&format!("    restart: {v}"))).services["a"].restart;
    assert_eq!(policy("always"), RestartPolicy::Always);
    assert_eq!(policy("unless-stopped"), RestartPolicy::UnlessStopped);
    assert_eq!(policy("on-failure"), RestartPolicy::OnFailure { max: None });
    assert_eq!(policy("\"on-failure:3\""), RestartPolicy::OnFailure { max: Some(3) });
    assert_eq!(policy("\"no\""), RestartPolicy::No);
    assert_eq!(policy("no"), RestartPolicy::No, "unquoted no is a string for this parser");
    assert_eq!(policy("false"), RestartPolicy::No);
    assert_eq!(ok(&svc("")).services["a"].restart, RestartPolicy::No);
    for (v, needle) in [
        ("sometimes", "invalid restart policy 'sometimes'"),
        ("on-failure:0", "at least 1"),
        ("true", "must be one of"),
        ("[always]", "must be one of"),
    ] {
        let e = err(&svc(&format!("    restart: {v}")));
        assert!(e.contains("service 'a': restart:") && e.contains(needle), "{v}: {e}");
    }
}

#[test]
fn restart_may_come_from_an_interpolated_value() {
    let p = parse_compose(&svc("    restart: ${POLICY:-on-failure:2}"), &env).unwrap();
    assert_eq!(p.services["a"].restart, RestartPolicy::OnFailure { max: Some(2) });
}

#[test]
fn x_yarc_sets_the_user_and_wins_over_the_generic_key() {
    assert_eq!(ok(&svc("    x-yarc: {user: root}")).services["a"].user.as_deref(), Some("root"));
    assert_eq!(ok(&svc("    x-yarc:\n      user: 0")).services["a"].user.as_deref(), Some("0"));
    assert_eq!(ok(&svc("    user: pms\n    x-yarc: {user: root}")).services["a"].user.as_deref(), Some("root"));
    assert_eq!(ok(&svc("    user: pms\n    x-yarc: {}")).services["a"].user.as_deref(), Some("pms"));
    assert_eq!(ok(&svc("    x-other: 1")).services["a"].user, None);
    let e = err(&svc("    x-yarc: {memory: 1g}"));
    assert_eq!(e, "service 'a' x-yarc: unsupported key 'memory'");
    assert!(err(&svc("    x-yarc: {user: ''}")).contains("user"));
    assert!(err(&svc("    x-yarc: [root]")).contains("x-yarc"));
}

#[test]
fn service_level_limits() {
    let r = ok(&svc("    mem_limit: 512m\n    memswap_limit: 768m\n    cpus: 0.5\n    pids_limit: 200")).services["a"]
        .resources;
    assert_eq!(r.memory, Some(512 << 20));
    assert_eq!(r.memory_swap, Some(768 << 20));
    assert_eq!((r.cpus_millis, r.pids), (Some(500), Some(200)));
    assert!(ok(&svc("")).services["a"].resources.is_unset());
    // numbers are bytes / CPUs, and -1 processes means unlimited
    let r = ok(&svc("    mem_limit: 134217728\n    cpus: 2\n    pids_limit: -1")).services["a"].resources;
    assert_eq!((r.memory, r.cpus_millis, r.pids), (Some(128 << 20), Some(2000), None));
}

#[test]
fn deploy_resources_limits_are_the_same_limits() {
    let deploy = "    deploy:\n      resources:\n        limits:\n          memory: 1g\n          cpus: '1.5'\n          pids: 50\n";
    let r = ok(&svc(deploy)).services["a"].resources;
    assert_eq!((r.memory, r.cpus_millis, r.pids), (Some(1 << 30), Some(1500), Some(50)));
    // the same value in both places is fine; different values are not
    let both = format!("{deploy}    mem_limit: 1g\n");
    assert_eq!(ok(&svc(&both)).services["a"].resources.memory, Some(1 << 30));
    let clash = format!("{deploy}    mem_limit: 2g\n");
    assert!(err(&svc(&clash)).contains("also set elsewhere"), "{}", err(&svc(&clash)));
    // reservations have nothing to reserve: accepted, ignored
    let reserved =
        "    deploy:\n      resources:\n        reservations:\n          memory: 64m\n          cpus: '0.25'\n";
    assert!(ok(&svc(reserved)).services["a"].resources.is_unset());
}

#[test]
fn other_deploy_keys_and_resource_keys_still_fail_loudly() {
    for (extra, needle) in [
        ("    deploy:\n      replicas: 2", "service 'a' deploy: unsupported key 'replicas'"),
        ("    deploy:\n      resources:\n        limits:\n          devices: x", "unsupported key 'devices'"),
        ("    deploy:\n      resources:\n        reservations:\n          devices: x", "unsupported key 'devices'"),
        ("    deploy:\n      resources:\n        extra: x", "unsupported key 'extra'"),
        ("    cpu_shares: 512", "unsupported key 'cpu_shares'"),
        ("    cpuset: '0'", "unsupported key 'cpuset'"),
        ("    mem_reservation: 64m", "unsupported key 'mem_reservation'"),
        ("    ulimits: {}", "unsupported key 'ulimits'"),
    ] {
        let e = err(&svc(extra));
        assert!(e.contains(needle), "{extra}: {e}");
    }
}

#[test]
fn bad_limits_name_the_key_and_the_value_only() {
    for (extra, needle) in [
        ("    mem_limit: lots", "service 'a': mem_limit: invalid size 'lots'"),
        ("    mem_limit: 1m", "service 'a': mem_limit: '1m' is too small"),
        ("    cpus: 0", "service 'a': cpus: '0' must be at least"),
        ("    cpus: [1]", "service 'a': cpus: must be a number"),
        ("    pids_limit: 0", "service 'a': pids_limit: invalid value '0'"),
        ("    memswap_limit: 1g", "service 'a': memswap_limit: needs a memory limit"),
        ("    mem_limit: 1g\n    memswap_limit: 512m", "memswap_limit: is memory plus swap"),
        (
            "    deploy:\n      resources:\n        limits:\n          memory: x",
            "deploy.resources.limits.memory: invalid size",
        ),
    ] {
        let e = err(&svc(extra));
        assert!(e.contains(needle), "{extra}: {e}");
    }
}

#[test]
fn limits_may_come_from_interpolated_values() {
    let p = parse_compose(&svc("    mem_limit: ${MEM:-256m}\n    cpus: ${CPUS:-1}"), &env).unwrap();
    assert_eq!(
        (p.services["a"].resources.memory, p.services["a"].resources.cpus_millis),
        (Some(256 << 20), Some(1000))
    );
}
