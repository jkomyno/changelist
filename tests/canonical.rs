//! Approval identity remains compatible when dependencies enable JSON map features.

use codemode::{Approval, Digest, Plan, ProviderDeclaration, RecoveryPolicy, approval_binding};

#[test]
fn plans_and_both_approval_formats_match_historical_canonical_vectors() {
    let plan = Plan::parse(r#"{"version":1,"operations":[{"id":"a","tool":"t","scope":{"provider":"p","account":"x"},"arguments":{"z":[{"b":2,"a":1}],"issue":1,"label":"bug"}}]}"#)
        .expect("plan");
    let declaration: ProviderDeclaration = serde_json::from_str(r#"{"provider":"p","account":"x","contracts":{"t":{"repeat":"unsafe","reconcile":"none"}},"retention":"metadata/1"}"#).expect("declaration");
    let policy = RecoveryPolicy {
        concurrency: 4,
        max_attempts: 3,
        max_probes: 3,
        attempt_timeout_ms: 10_000,
        probe_timeout_ms: 5_000,
        session_timeout_ms: 60_000,
    };
    // Vectors independently computed with Python sorted JSON and hashlib.sha256.
    assert_eq!(
        plan.canonical(),
        r#"{"operations":[{"arguments":{"issue":1,"label":"bug","z":[{"a":1,"b":2}]},"id":"a","scope":{"account":"x","provider":"p"},"tool":"t"}],"version":1}"#
    );
    assert_eq!(
        plan.digest().as_str(),
        "sha256:7cd0f9877f33f026ae43b245d476b00f27b864d1a62f837326ccd3771cd39b45"
    );
    assert_eq!(
        approval_binding(plan.digest(), &declaration, &policy).as_str(),
        "sha256:d2fcef73cc86e69d4f7d55957a6925aa174b67529f080b9506f36d9e29f0d5e6"
    );
    let legacy = Approval {
        binding: serde_json::from_str::<Digest>(
            "\"sha256:64087352c90ab354c722c0ee5284aa73b5e8db84daf78bd4196a03f5f8999a08\"",
        )
        .expect("digest"),
        plan: plan.digest().clone(),
        declaration,
        policy,
        approver: "test".into(),
        note: None,
        approved_unix_ms: 0,
    };
    assert!(legacy.predates_retention());
}

#[test]
fn decimal_values_do_not_depend_on_embedding_dependency_features() {
    for number in ["51.248178375505404", "51.24817837550541"] {
        let source = format!(
            r#"{{"version":1,"operations":[{{"id":"a","tool":"t","scope":{{"provider":"p","account":"x"}},"arguments":{{"latitude":{number}}}}}]}}"#
        );
        let plan = Plan::parse(&source).expect("decimal plan");
        assert!(
            plan.canonical().contains(number),
            "{number}: {}",
            plan.canonical()
        );
        let reopened = Plan::parse(plan.canonical()).expect("historical canonical decimal reopens");
        assert_eq!(plan.digest(), reopened.digest());
    }
}

#[test]
fn reserved_json_marker_keys_remain_literal_plan_data() {
    let source = r#"{"version":1,"operations":[{"id":"a","tool":"t","scope":{"provider":"p","account":"x"},"arguments":{"$serde_json::private::RawValue":"{\"x\":1}"},"preconditions":[{"nested":[{"$serde_json::private::RawValue":"not JSON"}]}]}]}"#;
    let plan = Plan::parse(source).expect("literal JSON keys");
    assert_eq!(
        plan.plan().operations[0].arguments["$serde_json::private::RawValue"],
        "{\"x\":1}"
    );
    assert_eq!(
        plan.plan().operations[0].preconditions[0]["nested"][0]["$serde_json::private::RawValue"],
        "not JSON"
    );
    assert_eq!(
        Plan::parse(plan.canonical())
            .expect("journal plan reopens")
            .digest(),
        plan.digest()
    );
}

#[tokio::test]
async fn literal_marker_objects_survive_native_and_program_protocols() {
    use codemode::{MockProvider, NativeCall, RunCancellation, RunConfig, RunStatus};
    let expected = serde_json::json!({"$serde_json::private::RawValue":"not JSON"});
    assert_eq!(
        codemode::parse_json(r#"{"$serde_json::private::RawValue":"not JSON"}"#)
            .expect("literal document"),
        expected
    );
    assert!(codemode::parse_json("{} {}").is_err());
    let request: NativeCall = serde_json::from_str(
        r#"{"tool":"mock.echo","arguments":{"$serde_json::private::RawValue":"not JSON"}}"#,
    )
    .expect("literal native request");
    let native = codemode::run_calls(
        &[request],
        RunConfig::default(),
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("native run");
    assert!(
        matches!(&native.calls[0].outcome, codemode::CallOutcome::Succeeded {value} if value == &expected)
    );

    let mut configs = vec![RunConfig::default()];
    #[cfg(feature = "embedded-v8")]
    configs.push(RunConfig {
        backend: codemode::GuestBackend::EmbeddedV8 {
            executable: env!("CARGO_BIN_EXE_codemode").into(),
        },
        ..RunConfig::default()
    });
    for config in configs.drain(..) {
        let report = codemode::run(
            "return await tools.call('mock.echo', {'$serde_json::private::RawValue':'not JSON'});",
            config,
            MockProvider,
            RunCancellation::default(),
        )
        .await
        .expect("program run");
        assert_eq!(report.status, RunStatus::Success, "{report:?}");
        assert_eq!(report.output.as_ref(), Some(&expected));
        let reopened: codemode::RunReport =
            serde_json::from_str(&serde_json::to_string(&report).expect("report JSON"))
                .expect("literal saved report");
        assert_eq!(reopened.output.as_ref(), Some(&expected));
    }
}

#[test]
fn generic_serde_numbers_keep_the_existing_value_contract() {
    use serde::de::value::{Error, F64Deserializer, I128Deserializer, U128Deserializer};
    assert_eq!(
        codemode::deserialize_json(U128Deserializer::<Error>::new(u64::MAX.into()))
            .expect("representable u128"),
        serde_json::json!(u64::MAX)
    );
    assert_eq!(
        codemode::deserialize_json(I128Deserializer::<Error>::new(i64::MIN.into()))
            .expect("representable i128"),
        serde_json::json!(i64::MIN)
    );
    assert!(codemode::deserialize_json(U128Deserializer::<Error>::new(u128::MAX)).is_err());
    assert_eq!(
        codemode::deserialize_json(F64Deserializer::<Error>::new(f64::INFINITY))
            .expect("nonfinite serde value"),
        serde_json::Value::Null
    );
}
