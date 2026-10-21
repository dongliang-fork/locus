use super::*;

const HEADER: &str =
    "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!114 &11400000\nMonoBehaviour:\n";

fn asset(body: &str) -> Vec<u8> {
    format!("{HEADER}{body}").into_bytes()
}
fn include_all(session: &MergeSession) -> Vec<Decision> {
    session
        .catalog()
        .changes
        .iter()
        .map(|c| Decision {
            change_id: c.id.clone(),
            resolution: Resolution::Include,
        })
        .collect()
}
fn rendered(base: &[u8], target: &[u8], source: &[u8]) -> MergeOutput {
    let session = prepare_merge(base, target, source).unwrap();
    session.render(&include_all(&session)).unwrap()
}

fn graph() -> Vec<u8> {
    asset(concat!(
        "  m_Script: {fileID: 11500000, guid: aabbccdd00112233445566778899aabb, type: 3}\n",
        "  localValue: 10\n  incomingValue: 20\n  unknownFutureField: {x: 19, y: 23}\n",
        "  root: {rid: 101}\n  alias: {rid: 9007199254740993}\n  optional: {rid: -2}\n",
        "  references:\n    version: 2\n    RefIds:\n",
        "    - rid: 101\n      type: {class: MergeGroup, ns: Locus.MergeTesting, asm: Locus}\n      data:\n",
        "        label: Root\n        next: {rid: 9007199254740993}\n        children:\n",
        "        - rid: 9007199254740993\n        - rid: 103\n        - rid: 9007199254740993\n",
        "    - rid: 9007199254740993\n      type: {class: MergeAction, ns: Locus.MergeTesting, asm: Locus}\n      data:\n",
        "        label: Shared\n        next: {rid: 101}\n        health: 100\n        speed: 1\n",
        "    - rid: 103\n      type: {class: MergeAction, ns: Locus.MergeTesting, asm: Locus}\n      data:\n",
        "        label: Alternate\n        next: {rid: -2}\n        health: 30\n        speed: 3\n",
    ))
}

fn replace(bytes: &[u8], old: &str, new: &str) -> Vec<u8> {
    String::from_utf8(bytes.to_vec())
        .unwrap()
        .replace(old, new)
        .into_bytes()
}

#[test]
fn no_op_is_byte_exact_with_bom_crlf_unknown_fields_and_comments() {
    let bytes=format!("\u{feff}{}",String::from_utf8(asset("  unknown: \"value: # literal\" # comment\n  empty:\n  null: null\n  list: [1, '2', {a: 3}]\n  block: |-\n    one: literal\n    # text\n\n")).unwrap().replace('\n',"\r\n")).into_bytes();
    let parsed = parse(&bytes).unwrap();
    assert_eq!(parsed.bytes.as_ref(), bytes.as_slice());
    let session = prepare_merge(&bytes, &bytes, &bytes).unwrap();
    assert!(session.catalog().changes.is_empty());
    assert_eq!(session.render(&[]).unwrap().bytes, bytes);
}

#[test]
fn parses_nested_flow_quotes_wrapped_plain_strings_and_indented_sequences() {
    let bytes=asset(concat!(
        "  nested: {first: [1, {label: 'a, b: it''s', ref: {fileID: 0}}],\n    second: \"escaped \\\" Unicode \\u4e2d\"}\n",
        "  'key: with spaces': value\n  wrapped: packed\n    continuation\n",
        "  list:\n    - name: one\n      nested:\n        - value: 1\n        - value: 2\n    - name: two\n      nested: []\n",
        "  tail: final\n",
    ));
    let parsed = parse(&bytes).unwrap();
    let body = parsed.documents[0].root.get("MonoBehaviour").unwrap();
    assert_eq!(body.get("list").unwrap().items().unwrap().len(), 2);
    assert!(body
        .get("nested")
        .unwrap()
        .get("first")
        .unwrap()
        .items()
        .is_some());
    assert_eq!(body.get("tail").unwrap().scalar(&parsed), Some("final"));
}

#[test]
fn missing_empty_null_and_quoted_empty_are_distinct() {
    let base = asset("  value:\n  emptyString: \"\"\n  explicitNull: null\n");
    let source = asset("  value: null\n  emptyString:\n");
    let session = prepare_merge(&base, &base, &source).unwrap();
    assert_eq!(session.catalog().changes.len(), 3);
    let result = session.render(&include_all(&session)).unwrap();
    assert!(result.ready);
    assert_eq!(result.bytes, source);
    let body = parse(&base).unwrap();
    let map = body.documents[0].root.get("MonoBehaviour").unwrap();
    assert_ne!(
        map.get("value").unwrap().fingerprint,
        map.get("emptyString").unwrap().fingerprint
    );
    assert_ne!(
        map.get("value").unwrap().fingerprint,
        map.get("explicitNull").unwrap().fingerprint
    );
}

#[test]
fn flow_vector_fields_merge_independently_and_preserve_unselected_bytes() {
    let base = asset("  position: {x: 0, y: 0, z: 0}\n  hidden: untouched # keep\n");
    let target = replace(&base, "x: 0", "x: 3");
    let source = replace(&base, "y: 0", "y: 4");
    let result = rendered(&base, &target, &source);
    assert!(result.ready);
    assert_eq!(result.bytes, replace(&target, "y: 0", "y: 4"));
    let session = prepare_merge(&base, &target, &source).unwrap();
    assert_eq!(session.render(&[]).unwrap().bytes, target);
}

#[test]
fn shared_cyclic_serialize_reference_graph_merges_fields_without_id_rounding() {
    let base = graph();
    let target = replace(
        &replace(&base, "health: 100", "health: 75"),
        "localValue: 10",
        "localValue: 99",
    );
    let source = replace(
        &replace(
            &replace(&base, "speed: 1\n", "speed: 2.5\n"),
            "incomingValue: 20",
            "incomingValue: 88",
        ),
        "label: Alternate",
        "label: Selected alternate",
    );
    let session = prepare_merge(&base, &target, &source).unwrap();
    assert_eq!(session.catalog().changes.len(), 3);
    assert!(session
        .catalog()
        .changes
        .iter()
        .any(|c| c.property_path.contains("@rid=9007199254740993/data/speed")));
    let result = session.render(&include_all(&session)).unwrap();
    assert!(result.ready, "{:?}", result.conflicts);
    let text = String::from_utf8(result.bytes.clone()).unwrap();
    assert!(text.contains("health: 75"));
    assert!(text.contains("speed: 2.5"));
    assert!(text.contains("localValue: 99"));
    assert!(text.contains("incomingValue: 88"));
    assert!(text.contains("unknownFutureField: {x: 19, y: 23}"));
    assert_eq!(
        parse(&result.bytes)
            .unwrap()
            .references()
            .iter()
            .filter(|r| r.rid.as_deref() == Some("9007199254740993"))
            .count(),
        4
    );
    assert!(validate(&parse(&result.bytes).unwrap()).is_empty());
}

#[test]
fn agent_selects_conflict_free_subset_and_ids_do_not_depend_on_target() {
    let base = graph();
    let target = replace(&base, "localValue: 10", "localValue: 99");
    let source = replace(
        &replace(&base, "speed: 1\n", "speed: 2.5\n"),
        "incomingValue: 20",
        "incomingValue: 88",
    );
    let session = prepare_merge(&base, &target, &source).unwrap();
    let clean = prepare_merge(&base, &base, &source).unwrap();
    assert_eq!(
        session
            .catalog()
            .changes
            .iter()
            .map(|c| &c.id)
            .collect::<Vec<_>>(),
        clean
            .catalog()
            .changes
            .iter()
            .map(|c| &c.id)
            .collect::<Vec<_>>()
    );
    let selected = session
        .catalog()
        .changes
        .iter()
        .find(|c| c.property_path.ends_with("/speed"))
        .unwrap();
    let result = session
        .render(&[Decision {
            change_id: selected.id.clone(),
            resolution: Resolution::Include,
        }])
        .unwrap();
    assert_eq!(result.bytes, replace(&target, "speed: 1\n", "speed: 2.5\n"));
}

#[test]
fn conflicting_field_requires_explicit_agent_resolution() {
    let base = graph();
    let target = replace(&base, "health: 100", "health: 75");
    let source = replace(&base, "health: 100", "health: 42");
    let session = prepare_merge(&base, &target, &source).unwrap();
    let change = &session.catalog().changes[0];
    assert_eq!(change.status, ChangeStatus::Conflict);
    let blocked = session.render(&include_all(&session)).unwrap();
    assert!(!blocked.ready);
    assert_eq!(blocked.bytes, target);
    let resolved = session
        .render(&[Decision {
            change_id: change.id.clone(),
            resolution: Resolution::Source,
        }])
        .unwrap();
    assert!(resolved.ready);
    assert_eq!(resolved.bytes, source);
}

#[test]
fn type_change_and_old_type_data_edit_are_one_conflict() {
    let base = graph();
    let target = replace(&base, "health: 100", "health: 75");
    let source = replace(
        &replace(&base, "class: MergeAction", "class: MergeWeightedAction"),
        "speed: 1\n",
        "weight: 9\n",
    );
    let session = prepare_merge(&base, &target, &source).unwrap();
    let changed = session
        .catalog()
        .changes
        .iter()
        .find(|c| c.property_path.ends_with("@rid=9007199254740993"))
        .unwrap();
    assert_eq!(changed.status, ChangeStatus::Conflict);
    assert!(changed.reason.as_deref().unwrap().contains("type identity"));
    let blocked = session.render(&include_all(&session)).unwrap();
    assert!(!blocked.ready);
    let selected = session
        .render(&[Decision {
            change_id: changed.id.clone(),
            resolution: Resolution::Source,
        }])
        .unwrap();
    assert!(selected.ready);
    let text = String::from_utf8(selected.bytes).unwrap();
    assert!(text.contains("weight: 9"));
    assert!(text.contains("health: 100"));
}

#[test]
fn identity_list_reordering_cannot_silently_apply_positional_edits() {
    let base = graph();
    let target = replace(&base, "health: 100", "health: 75");
    let source = replace(
        &base,
        "        - rid: 103\n        - rid: 9007199254740993",
        "        - rid: 9007199254740993\n        - rid: 103",
    );
    let session = prepare_merge(&base, &target, &source).unwrap();
    // Shared-reference arrays contain duplicates, so list order is an atomic
    // field; editing a different registry object remains independently safe.
    let result = session.render(&include_all(&session)).unwrap();
    assert!(result.ready);
    assert!(String::from_utf8(result.bytes)
        .unwrap()
        .contains("health: 75"));
    let target_reordered = replace(
        &base,
        "        - rid: 9007199254740993\n        - rid: 103",
        "        - rid: 103\n        - rid: 9007199254740993",
    );
    assert!(!rendered(&base, &target_reordered, &source).ready);
}

#[test]
fn ordinary_duplicate_value_arrays_conflict_instead_of_index_pairing() {
    let base = asset("  values: [1, 1, 2]\n");
    let target = asset("  values: [1, 2, 1]\n");
    let source = asset("  values: [9, 1, 2]\n");
    let result = rendered(&base, &target, &source);
    assert!(!result.ready);
    assert_eq!(result.bytes, target);
}

#[test]
fn pptr_guid_and_file_id_cannot_form_a_synthetic_reference() {
    let base =
        asset("  reference: {fileID: 100, guid: aabbccdd00112233445566778899aabb, type: 3}\n");
    let target = replace(&base, "fileID: 100", "fileID: 200");
    let source = replace(
        &base,
        "aabbccdd00112233445566778899aabb",
        "112233445566778899aabbccddeeff00",
    );
    let session = prepare_merge(&base, &target, &source).unwrap();
    assert_eq!(session.catalog().changes.len(), 1);
    assert!(!session.render(&include_all(&session)).unwrap().ready);
}

#[test]
fn selecting_reference_without_its_registry_addition_returns_dependency_conflict() {
    let base = graph();
    let source = replace(&base, "optional: {rid: -2}", "optional: {rid: 104}");
    let session = prepare_merge(&base, &base, &source).unwrap();
    let result = session.render(&include_all(&session)).unwrap();
    assert!(!result.ready);
    assert!(result
        .conflicts
        .iter()
        .any(|c| c.code == "dangling_managed_reference"));
    assert_eq!(result.bytes, base);
}

#[test]
fn deleting_a_referenced_object_is_blocked_until_owner_reference_is_removed() {
    let base=b"--- !u!1 &1\nGameObject:\n  m_Component:\n  - component: {fileID: 2}\n--- !u!114 &2\nMonoBehaviour:\n  m_GameObject: {fileID: 1}\n  value: 0\n";
    let source = b"--- !u!1 &1\nGameObject:\n  m_Component: []\n";
    let session = prepare_merge(base, base, source).unwrap();
    let deletion = session
        .catalog()
        .changes
        .iter()
        .find(|c| c.kind == ChangeKind::RemoveObject)
        .unwrap();
    let blocked = session
        .render(&[Decision {
            change_id: deletion.id.clone(),
            resolution: Resolution::Include,
        }])
        .unwrap();
    assert!(!blocked.ready);
    assert!(session.render(&include_all(&session)).unwrap().ready);
}

#[test]
fn duplicate_keys_rids_objects_and_unsupported_syntax_never_silently_write() {
    let duplicate = asset("  value: 1\n  value: 2\n");
    let parsed = parse(&duplicate).unwrap();
    assert!(!parsed.is_writable());
    assert!(prepare_merge(&duplicate, &duplicate, &duplicate).is_err());
    let duplicate_rid = replace(&graph(), "rid: 103\n      type:", "rid: 101\n      type:");
    assert!(validate(&parse(&duplicate_rid).unwrap())
        .iter()
        .any(|d| d.code == "duplicate_rid"));
    assert!(parse(&asset("  value: *alias\n")).is_err());
    assert!(parse(&asset("  value: [1, 2\n")).is_err());
    assert!(parse(b"\xff\x00").is_err());
    let repeated = b"--- !u!1 &1\nGameObject:\n  value: 1\n--- !u!1 &1\nGameObject:\n  value: 2\n";
    assert!(!parse(repeated).unwrap().is_writable());
}

#[test]
fn typed_field_set_quotes_content_and_never_injects_yaml() {
    let base = asset("  name: before\n  preserve: 19\n");
    let source = asset("  name: after\n  preserve: 19\n");
    let session = prepare_merge(&base, &base, &source).unwrap();
    let result = session
        .render(&[Decision {
            change_id: session.catalog().changes[0].id.clone(),
            resolution: Resolution::Set {
                value: serde_json::json!("new\n  preserve: 0"),
            },
        }])
        .unwrap();
    assert!(result.ready);
    let text = String::from_utf8(result.bytes).unwrap();
    assert!(text.contains("name: \"new\\n  preserve: 0\""));
    assert!(text.contains("\n  preserve: 19\n"));
}

#[test]
fn additions_deletions_and_last_line_without_newline_preserve_other_fields() {
    let base = asset("  remove: 1\n  keep: 2");
    let target = replace(&base, "keep: 2", "keep: 5");
    let source = asset("  keep: 2\n  added: {nested: [1, 2]}\n");
    let result = rendered(&base, &target, &source);
    assert!(result.ready);
    assert_eq!(
        result.bytes,
        asset("  keep: 5\n  added: {nested: [1, 2]}\n")
    );
}

#[test]
fn object_and_file_selection_reject_overlapping_field_decisions() {
    let base = graph();
    let source = replace(&base, "incomingValue: 20", "incomingValue: 88");
    let session = prepare_merge(&base, &base, &source).unwrap();
    let mut decisions = include_all(&session);
    decisions.push(MergeSession::object_decision(
        "11400000",
        Resolution::Source,
    ));
    assert!(session.render(&decisions).is_err());
    assert_eq!(
        session
            .render(&[MergeSession::object_decision(
                "11400000",
                Resolution::Source
            )])
            .unwrap()
            .bytes,
        source
    );
    assert_eq!(
        session
            .render(&[MergeSession::file_decision(Resolution::Source)])
            .unwrap()
            .bytes,
        source
    );
}

#[test]
fn prefab_overrides_pair_by_target_and_property_path_and_stripped_objects_survive() {
    let base=concat!("--- !u!1001 &10\nPrefabInstance:\n  m_Modification:\n    m_Modifications:\n",
        "    - target: {fileID: 11, guid: aabbccdd00112233445566778899aabb, type: 3}\n      propertyPath: m_LocalPosition.x\n      value: 0\n      objectReference: {fileID: 0}\n",
        "    - target: {fileID: 11, guid: aabbccdd00112233445566778899aabb, type: 3}\n      propertyPath: m_LocalPosition.y\n      value: 0\n      objectReference: {fileID: 0}\n",
        "--- !u!4 &11 stripped\nTransform:\n  m_CorrespondingSourceObject: {fileID: 55, guid: aabbccdd00112233445566778899aabb, type: 3}\n  m_PrefabInstance: {fileID: 10}\n").as_bytes();
    let target = replace(
        base,
        "m_LocalPosition.x\n      value: 0",
        "m_LocalPosition.x\n      value: 3",
    );
    let source = replace(
        base,
        "m_LocalPosition.y\n      value: 0",
        "m_LocalPosition.y\n      value: 4",
    );
    let result = rendered(base, &target, &source);
    assert!(result.ready);
    assert_eq!(
        result.bytes,
        replace(
            &target,
            "m_LocalPosition.y\n      value: 0",
            "m_LocalPosition.y\n      value: 4"
        )
    );
    assert!(
        parse(&result.bytes)
            .unwrap()
            .document("11")
            .unwrap()
            .stripped
    );
}

#[test]
fn parser_limits_are_enforced_before_unbounded_recursion() {
    let bytes = asset("  nested: {one: {two: {three: 1}}}\n");
    assert!(Asset::parse_with_limits(
        &bytes,
        Limits {
            max_depth: 2,
            ..Limits::default()
        }
    )
    .is_err());
    assert!(Asset::parse_with_limits(
        &bytes,
        Limits {
            max_nodes: 2,
            ..Limits::default()
        }
    )
    .is_err());
    assert!(Asset::parse_with_limits(
        &bytes,
        Limits {
            max_bytes: 4,
            ..Limits::default()
        }
    )
    .is_err());
}

#[test]
fn serde_contract_uses_string_ids_and_explicit_typed_resolution() {
    let resolution: Resolution =
        serde_json::from_value(serde_json::json!({"kind":"set","value":"9007199254740993"}))
            .unwrap();
    assert!(matches!(
        resolution,
        Resolution::Set {
            value: serde_json::Value::String(_)
        }
    ));
    let session = prepare_merge(
        &graph(),
        &graph(),
        &replace(&graph(), "health: 100", "health: 101"),
    )
    .unwrap();
    let json = serde_json::to_value(session.catalog()).unwrap();
    assert_eq!(json["changes"][0]["object_id"], "11400000");
}

#[test]
fn proven_formerly_serialized_as_migrates_only_selected_values() {
    let base = asset("  health: 100\n  local: 7\n");
    let target = asset("  hitPoints: 100\n  local: 99\n");
    let source = asset("  health: 120\n  local: 7\n");
    let alias = FieldAlias {
        object_id: "11400000".into(),
        old_path: "/MonoBehaviour/health".into(),
        new_path: "/MonoBehaviour/hitPoints".into(),
    };
    let session = prepare_merge_with_aliases(&base, &target, &source, &[alias.clone()]).unwrap();
    assert_eq!(session.render(&[]).unwrap().bytes, target);
    assert_eq!(session.catalog().changes.len(), 1);
    assert_eq!(
        session.catalog().changes[0].property_path,
        "/MonoBehaviour/hitPoints"
    );
    let result = session.render(&include_all(&session)).unwrap();
    assert!(result.ready);
    assert_eq!(result.bytes, asset("  hitPoints: 120\n  local: 99\n"));
    assert_eq!(
        session
            .render(&[MergeSession::file_decision(Resolution::Source)])
            .unwrap()
            .bytes,
        source
    );
    let duplicate = asset("  health: 100\n  hitPoints: 50\n  local: 7\n");
    assert!(prepare_merge_with_aliases(&duplicate, &target, &source, &[alias]).is_err());
}

#[test]
fn explicit_typed_fields_work_without_source_deltas_and_insert_only_in_existing_parent() {
    let base = asset("  value: 1\n  rows:\n  - name: old\n    amount: 3\n");
    let session = prepare_merge(&base, &base, &base).unwrap();
    let decisions = vec![
        MergeSession::field_decision(
            "11400000",
            "/MonoBehaviour/value",
            Resolution::Set {
                value: serde_json::json!(9),
            },
        ),
        MergeSession::field_decision(
            "11400000",
            "/MonoBehaviour/added",
            Resolution::Set {
                value: serde_json::json!({"x":1,"y":2}),
            },
        ),
        MergeSession::field_decision(
            "11400000",
            "/MonoBehaviour/rows/0/name",
            Resolution::Set {
                value: serde_json::json!("new"),
            },
        ),
    ];
    let result = session.render(&decisions).unwrap();
    assert!(result.ready);
    let text = String::from_utf8(result.bytes).unwrap();
    assert!(text.contains("  value: 9\n"));
    assert!(text.contains("  added: {x: 1, y: 2}\n"));
    // A directly addressed string is written as a typed, quoted value.
    assert!(text.contains("  - name: \"new\"\n"));
    assert!(session
        .render(&[MergeSession::field_decision(
            "11400000",
            "/MonoBehaviour/missing/child",
            Resolution::Set {
                value: serde_json::json!(1)
            }
        )])
        .is_err());
}

#[test]
fn empty_sequence_elements_remain_separate_and_unknown_registry_data_is_preserved() {
    let bytes = asset("  strings:\n  - first\n  - \n  - second\n  - \n  tail: done\n");
    let parsed = parse(&bytes).unwrap();
    assert_eq!(
        parsed.documents[0]
            .root
            .get("MonoBehaviour")
            .unwrap()
            .get("strings")
            .unwrap()
            .items()
            .unwrap()
            .len(),
        4
    );
    assert_eq!(
        prepare_merge(&bytes, &bytes, &bytes)
            .unwrap()
            .render(&[])
            .unwrap()
            .bytes,
        bytes
    );
}

#[test]
fn shared_parse_cache_reuses_syntax_without_cross_snapshot_state() {
    let bytes = graph();
    let a = parse_shared(&bytes).unwrap();
    let b = parse_shared(&bytes).unwrap();
    assert!(std::sync::Arc::ptr_eq(&a, &b));
    let changed = parse_shared(&replace(&bytes, "health: 100", "health: 105")).unwrap();
    assert!(!std::sync::Arc::ptr_eq(&a, &changed));
    assert!(a.text(a.documents[0].span).contains("health: 100"));
}

#[test]
fn object_reparent_requires_both_parent_lists_and_rejects_cycles() {
    let bytes=b"--- !u!4 &1\nTransform:\n  m_Father: {fileID: 0}\n  m_Children:\n  - {fileID: 3}\n--- !u!4 &2\nTransform:\n  m_Father: {fileID: 0}\n  m_Children: []\n--- !u!4 &3\nTransform:\n  m_Father: {fileID: 1}\n  m_Children: []\n";
    let session = prepare_merge(bytes, bytes, bytes).unwrap();
    let mut decisions = vec![MergeSession::field_decision(
        "3",
        "/Transform/m_Father",
        Resolution::Set {
            value: serde_json::json!({"fileID":2}),
        },
    )];
    assert!(!session.render(&decisions).unwrap().ready);
    decisions.push(MergeSession::field_decision(
        "1",
        "/Transform/m_Children",
        Resolution::Set {
            value: serde_json::json!([]),
        },
    ));
    decisions.push(MergeSession::field_decision(
        "2",
        "/Transform/m_Children",
        Resolution::Set {
            value: serde_json::json!([{"fileID":3}]),
        },
    ));
    let result = session.render(&decisions).unwrap();
    assert!(result.ready, "{:?}", result.conflicts);
    let cycle = replace(
        &replace(
            bytes,
            "m_Father: {fileID: 0}\n  m_Children:\n  - {fileID: 3}",
            "m_Father: {fileID: 3}\n  m_Children:\n  - {fileID: 3}",
        ),
        "--- !u!4 &3\nTransform:\n  m_Father: {fileID: 1}\n  m_Children: []",
        "--- !u!4 &3\nTransform:\n  m_Father: {fileID: 1}\n  m_Children:\n  - {fileID: 1}",
    );
    assert!(validate(&parse(&cycle).unwrap())
        .iter()
        .any(|d| d.code == "transform_cycle"));
}

#[test]
fn absent_git_snapshots_allow_added_and_removed_entire_files() {
    let bytes = asset("  value: 1\n");
    let addition =
        MergeSession::new(Asset::absent(), Asset::absent(), parse(&bytes).unwrap()).unwrap();
    let added = addition.render(&include_all(&addition)).unwrap();
    assert!(added.ready);
    assert!(String::from_utf8(added.bytes).unwrap().contains("value: 1"));
    let removal = MergeSession::new(
        parse(&bytes).unwrap(),
        parse(&bytes).unwrap(),
        Asset::absent(),
    )
    .unwrap();
    let removed = removal.render(&include_all(&removal)).unwrap();
    assert!(removed.ready);
    assert!(removed.bytes.is_empty());
    assert!(parse(&[]).is_err());
}

#[test]
fn independently_added_equal_ids_are_conflicts_even_with_equal_payload() {
    let bytes = asset("  value: 1\n");
    let session = MergeSession::new(
        Asset::absent(),
        parse(&bytes).unwrap(),
        parse(&bytes).unwrap(),
    )
    .unwrap();
    assert_eq!(session.catalog().changes[0].status, ChangeStatus::Conflict);
    let base = asset("  references:\n    version: 2\n    RefIds: []\n");
    let added=asset("  references:\n    version: 2\n    RefIds:\n    - rid: 1\n      type: {class: Empty, ns: Example, asm: Tests}\n      data: {}\n");
    let session = prepare_merge(&base, &added, &added).unwrap();
    assert_eq!(session.catalog().changes[0].status, ChangeStatus::Conflict);
}

// Unity-shaped PrefabInstance overrides: wrapped flow `target:` mappings and a
// `\u`-escaped name, as written by Unity 6.
fn prefab_instance(overrides: &[(&str, &str, &str)]) -> Vec<u8> {
    let mut body = String::from(concat!(
        "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1001 &1050404689\nPrefabInstance:\n",
        "  m_ObjectHideFlags: 0\n  serializedVersion: 2\n  m_Modification:\n    serializedVersion: 3\n",
        "    m_TransformParent: {fileID: 0}\n    m_Modifications:\n",
    ));
    for (target, property, value) in overrides {
        body.push_str(&format!(
            "    - target: {{fileID: {target}, guid: cba0fbbfd3cdbce499475af4af43a2e3,\n        type: 3}}\n      propertyPath: {property}\n      value: {value}\n      objectReference: {{fileID: 0}}\n"
        ));
    }
    body.push_str("    m_RemovedComponents: []\n  m_SourcePrefab: {fileID: 100100000, guid: cba0fbbfd3cdbce499475af4af43a2e3, type: 3}\n");
    body.into_bytes()
}

const ROOT_T: &str = "600249992675133040";
const NAME_T: &str = "1937218400633567911";
const VIEW_T: &str = "7468865217963383891";
const NAME_VALUE: &str = "\"C1010_\\u9B54\\u65CF\\u58EB\\u5175_01\"";

fn session_base() -> Vec<u8> {
    prefab_instance(&[
        (ROOT_T, "m_LocalPosition.x", "-126.74875"),
        (ROOT_T, "m_LocalRotation.x", "-0"),
        (ROOT_T, "m_LocalRotation.z", "-0"),
        (NAME_T, "m_Name", NAME_VALUE),
        (VIEW_T, "legacyRenderers.Array.size", "0"),
    ])
}

fn change_ending<'a>(session: &'a MergeSession, suffix: &str) -> &'a Change {
    session
        .catalog()
        .changes
        .iter()
        .find(|c| c.property_path.ends_with(suffix))
        .unwrap_or_else(|| panic!("no change ending with {suffix}: {:#?}", session.catalog().changes))
}

#[test]
fn prefab_override_membership_changes_merge_per_element() {
    // The destination removed one override; the source removed two others and
    // changed a value. Every element change is independent and clean.
    let base = session_base();
    let target = prefab_instance(&[
        (ROOT_T, "m_LocalPosition.x", "-126.74875"),
        (ROOT_T, "m_LocalRotation.x", "-0"),
        (ROOT_T, "m_LocalRotation.z", "-0"),
        (NAME_T, "m_Name", NAME_VALUE),
    ]);
    let source = prefab_instance(&[
        (ROOT_T, "m_LocalPosition.x", "-127.19"),
        (NAME_T, "m_Name", NAME_VALUE),
        (VIEW_T, "legacyRenderers.Array.size", "0"),
    ]);
    let session = prepare_merge(&base, &target, &source).unwrap();
    assert!(
        session
            .catalog()
            .changes
            .iter()
            .all(|c| c.status == ChangeStatus::Clean && c.kind != ChangeKind::ReplaceSequence),
        "{:#?}",
        session.catalog().changes
    );
    let result = session.render(&include_all(&session)).unwrap();
    assert!(result.ready, "{:?}", result.conflicts);
    assert_eq!(
        result.bytes,
        prefab_instance(&[
            (ROOT_T, "m_LocalPosition.x", "-127.19"),
            (NAME_T, "m_Name", NAME_VALUE),
        ])
    );
}

#[test]
fn same_override_changed_differently_conflicts_only_at_that_element() {
    let base = session_base();
    let target = replace(&base, "value: -126.74875", "value: -144.65");
    let source = replace(
        &replace(&base, "value: -126.74875", "value: -127.19"),
        "legacyRenderers.Array.size\n      value: 0",
        "legacyRenderers.Array.size\n      value: 1",
    );
    let session = prepare_merge(&base, &target, &source).unwrap();
    let position = change_ending(&session, ":m_LocalPosition.x/value");
    assert_eq!(position.status, ChangeStatus::Conflict);
    assert!(position.reason.as_deref().unwrap().contains("resolve side="));
    let legacy = change_ending(&session, ":legacyRenderers.Array.size/value");
    assert_eq!(legacy.status, ChangeStatus::Clean);
    let decisions = vec![
        Decision { change_id: position.id.clone(), resolution: Resolution::Target },
        Decision { change_id: legacy.id.clone(), resolution: Resolution::Include },
    ];
    let result = session.render(&decisions).unwrap();
    assert!(result.ready, "{:?}", result.conflicts);
    assert_eq!(
        result.bytes,
        replace(
            &target,
            "legacyRenderers.Array.size\n      value: 0",
            "legacyRenderers.Array.size\n      value: 1"
        )
    );
}

#[test]
fn deleted_and_modified_override_elements_conflict_and_restore_in_place() {
    let base = session_base();
    // The destination deleted the position override; the source modified it.
    let target = prefab_instance(&[
        (ROOT_T, "m_LocalRotation.x", "-0"),
        (ROOT_T, "m_LocalRotation.z", "-0"),
        (NAME_T, "m_Name", NAME_VALUE),
        (VIEW_T, "legacyRenderers.Array.size", "0"),
    ]);
    let source = prefab_instance(&[
        (ROOT_T, "m_LocalPosition.x", "-127.19"),
        (NAME_T, "m_Name", NAME_VALUE),
        (VIEW_T, "legacyRenderers.Array.size", "0"),
    ]);
    let session = prepare_merge(&base, &target, &source).unwrap();
    let restored = change_ending(&session, ":m_LocalPosition.x");
    assert_eq!(restored.status, ChangeStatus::Conflict);
    assert!(restored.reason.as_deref().unwrap().contains("destination deleted"));
    assert!(!session.render(&include_all(&session)).unwrap().ready);
    let decisions: Vec<_> = session
        .catalog()
        .changes
        .iter()
        .map(|c| Decision {
            change_id: c.id.clone(),
            resolution: if c.id == restored.id { Resolution::Source } else { Resolution::Include },
        })
        .collect();
    let result = session.render(&decisions).unwrap();
    assert!(result.ready, "{:?}", result.conflicts);
    assert_eq!(result.bytes, source);

    // The source deleted an override the destination modified.
    let target = replace(&base, "m_LocalRotation.x\n      value: -0", "m_LocalRotation.x\n      value: 0.5");
    let source = prefab_instance(&[
        (ROOT_T, "m_LocalPosition.x", "-126.74875"),
        (ROOT_T, "m_LocalRotation.z", "-0"),
        (NAME_T, "m_Name", NAME_VALUE),
        (VIEW_T, "legacyRenderers.Array.size", "0"),
    ]);
    let session = prepare_merge(&base, &target, &source).unwrap();
    let removal = change_ending(&session, ":m_LocalRotation.x");
    assert_eq!(
        (removal.status.clone(), removal.kind.clone()),
        (ChangeStatus::Conflict, ChangeKind::RemoveField)
    );
    assert!(removal.reason.as_deref().unwrap().contains("source deleted"));
}

#[test]
fn source_added_override_enters_after_its_source_neighbor() {
    let base = session_base();
    let target = replace(
        &base,
        "legacyRenderers.Array.size\n      value: 0",
        "legacyRenderers.Array.size\n      value: 2",
    );
    let source = prefab_instance(&[
        (ROOT_T, "m_LocalPosition.x", "-126.74875"),
        (ROOT_T, "m_LocalPosition.z", "-18.64"),
        (ROOT_T, "m_LocalRotation.x", "-0"),
        (ROOT_T, "m_LocalRotation.z", "-0"),
        (NAME_T, "m_Name", NAME_VALUE),
        (VIEW_T, "legacyRenderers.Array.size", "0"),
    ]);
    let session = prepare_merge(&base, &target, &source).unwrap();
    let added = change_ending(&session, ":m_LocalPosition.z");
    assert_eq!(
        (added.status.clone(), added.kind.clone()),
        (ChangeStatus::Clean, ChangeKind::AddField)
    );
    let result = session.render(&include_all(&session)).unwrap();
    assert!(result.ready, "{:?}", result.conflicts);
    assert_eq!(
        result.bytes,
        replace(
            &source,
            "legacyRenderers.Array.size\n      value: 0",
            "legacyRenderers.Array.size\n      value: 2"
        )
    );
}

#[test]
fn ordered_identity_lists_still_reject_source_reordering() {
    let base = b"--- !u!1 &1\nGameObject:\n  m_Component:\n  - component: {fileID: 2}\n  - component: {fileID: 3}\n--- !u!4 &2\nTransform:\n  m_GameObject: {fileID: 1}\n--- !u!114 &3\nMonoBehaviour:\n  m_GameObject: {fileID: 1}\n--- !u!114 &4\nMonoBehaviour:\n  m_GameObject: {fileID: 1}\n";
    let target = replace(
        base,
        "  - component: {fileID: 3}\n",
        "  - component: {fileID: 3}\n  - component: {fileID: 4}\n",
    );
    let source = replace(
        base,
        "  - component: {fileID: 2}\n  - component: {fileID: 3}\n",
        "  - component: {fileID: 3}\n  - component: {fileID: 2}\n",
    );
    let session = prepare_merge(base, &target, &source).unwrap();
    let change = change_ending(&session, "/m_Component");
    assert_eq!(
        (change.status.clone(), change.kind.clone()),
        (ChangeStatus::Conflict, ChangeKind::ReplaceSequence)
    );
    assert!(change.reason.as_deref().unwrap().contains("reordered"));
}

#[test]
fn quoted_identity_text_addresses_the_same_override() {
    let base = session_base();
    // A tool wrote the destination's propertyPath quoted.
    let target = replace(&base, "propertyPath: m_LocalRotation.x", "propertyPath: \"m_LocalRotation.x\"");
    let source = replace(&base, "m_LocalRotation.x\n      value: -0", "m_LocalRotation.x\n      value: 0.25");
    let session = prepare_merge(&base, &target, &source).unwrap();
    let change = change_ending(&session, ":m_LocalRotation.x/value");
    assert_eq!(change.status, ChangeStatus::Clean);
    let result = session.render(&include_all(&session)).unwrap();
    assert!(result.ready, "{:?}", result.conflicts);
    assert_eq!(
        result.bytes,
        replace(
            &target,
            "\"m_LocalRotation.x\"\n      value: -0",
            "\"m_LocalRotation.x\"\n      value: 0.25"
        )
    );
}

fn override_key(bytes: &[u8], target_file_id: &str, property: &str) -> String {
    let asset = parse(bytes).unwrap();
    let node = asset.documents[0]
        .root
        .get("PrefabInstance")
        .unwrap()
        .get("m_Modification")
        .unwrap()
        .get("m_Modifications")
        .unwrap();
    let keys = crate::unity_asset_core::sequence_keys(
        &asset,
        node,
        "/PrefabInstance/m_Modification/m_Modifications",
    )
    .unwrap();
    let index = node
        .items()
        .unwrap()
        .iter()
        .position(|item| {
            let file_id = item.value.get("target").unwrap().get("fileID").unwrap();
            asset.text(file_id.span) == target_file_id
                && asset.text(item.value.get("propertyPath").unwrap().span) == property
        })
        .unwrap();
    format!("/PrefabInstance/m_Modification/m_Modifications/{}", keys[index])
}

#[test]
fn sequence_elements_are_deleted_copied_and_inserted_explicitly() {
    let base = session_base();
    let target = prefab_instance(&[
        (ROOT_T, "m_LocalPosition.x", "-126.74875"),
        (NAME_T, "m_Name", NAME_VALUE),
        (VIEW_T, "legacyRenderers.Array.size", "0"),
    ]);
    let session = prepare_merge(&base, &target, &base).unwrap();
    let legacy = override_key(&base, VIEW_T, "legacyRenderers.Array.size");
    let deleted = session
        .render(&[MergeSession::field_decision("1050404689", &legacy, Resolution::Delete)])
        .unwrap();
    assert!(deleted.ready, "{:?}", deleted.conflicts);
    assert_eq!(
        deleted.bytes,
        prefab_instance(&[(ROOT_T, "m_LocalPosition.x", "-126.74875"), (NAME_T, "m_Name", NAME_VALUE)])
    );
    // Copy an element the destination lacks back from the base, in base order.
    let rotation = override_key(&base, ROOT_T, "m_LocalRotation.z");
    let restored = session
        .render(&[MergeSession::field_decision("1050404689", &rotation, Resolution::Base)])
        .unwrap();
    assert!(restored.ready, "{:?}", restored.conflicts);
    assert_eq!(
        restored.bytes,
        prefab_instance(&[
            (ROOT_T, "m_LocalPosition.x", "-126.74875"),
            (ROOT_T, "m_LocalRotation.z", "-0"),
            (NAME_T, "m_Name", NAME_VALUE),
            (VIEW_T, "legacyRenderers.Array.size", "0"),
        ])
    );
    // A typed element must carry the identity it is addressed by.
    let value = serde_json::json!({"target":{"fileID":600249992675133040i64,"guid":"cba0fbbfd3cdbce499475af4af43a2e3","type":3},
        "propertyPath":"m_LocalRotation.z","value":"-0","objectReference":{"fileID":0}});
    let inserted = session
        .render(&[MergeSession::field_decision(
            "1050404689",
            &rotation,
            Resolution::Set { value: value.clone() },
        )])
        .unwrap();
    assert!(inserted.ready, "{:?}", inserted.conflicts);
    let text = String::from_utf8(inserted.bytes).unwrap();
    assert!(
        text.contains("\n    - target: {fileID: 600249992675133040, guid: cba0fbbfd3cdbce499475af4af43a2e3, type: 3}\n      propertyPath: m_LocalRotation.z\n      value: -0\n      objectReference: {fileID: 0}\n    m_RemovedComponents"),
        "{text}"
    );
    let mismatch = session.render(&[MergeSession::field_decision(
        "1050404689",
        &legacy.replace("legacyRenderers", "other"),
        Resolution::Set { value },
    )]);
    assert_eq!(mismatch.unwrap_err().code, "identity_mismatch");
    // The first field of an element shares its `- ` line.
    let first = session.render(&[MergeSession::field_decision(
        "1050404689",
        &format!("{legacy}/target"),
        Resolution::Delete,
    )]);
    assert!(first.unwrap_err().message.contains("first field of a sequence element"));
    let flow = session.render(&[MergeSession::field_decision(
        "1050404689",
        &format!("{legacy}/target/type"),
        Resolution::Delete,
    )]);
    assert!(flow.unwrap_err().message.contains("flow collection"));
}

#[test]
fn typed_sequence_set_rewrites_only_changed_elements_and_keeps_spelling() {
    let base = session_base();
    let session = prepare_merge(&base, &base, &base).unwrap();
    // What a caller reads back as "source minus one element, one value changed".
    let item = |target: &str, property: &str, value: &str| {
        serde_json::json!({
            "target":{"fileID":target.parse::<i64>().unwrap(),"guid":"cba0fbbfd3cdbce499475af4af43a2e3","type":3},
            "propertyPath":property,"value":value,"objectReference":{"fileID":0}})
    };
    let value = serde_json::json!([
        item(ROOT_T, "m_LocalPosition.x", "-127.19"),
        item(ROOT_T, "m_LocalRotation.x", "-0"),
        item(ROOT_T, "m_LocalRotation.z", "-0"),
        item(NAME_T, "m_Name", "C1010_魔族士兵_01"),
    ]);
    let result = session
        .render(&[MergeSession::field_decision(
            "1050404689",
            "/PrefabInstance/m_Modification/m_Modifications",
            Resolution::Set { value },
        )])
        .unwrap();
    assert!(result.ready, "{:?}", result.conflicts);
    assert_eq!(
        result.bytes,
        prefab_instance(&[
            (ROOT_T, "m_LocalPosition.x", "-127.19"),
            (ROOT_T, "m_LocalRotation.x", "-0"),
            (ROOT_T, "m_LocalRotation.z", "-0"),
            (NAME_T, "m_Name", NAME_VALUE),
        ])
    );
}

#[test]
fn lines_copied_from_lf_sources_follow_a_crlf_destination() {
    let base = asset("  keep: 1\n");
    let target = String::from_utf8(base.clone())
        .unwrap()
        .replace('\n', "\r\n")
        .into_bytes();
    let source = asset("  keep: 1\n  added:\n    nested: 2\n");
    let result = rendered(&base, &target, &source);
    assert!(result.ready, "{:?}", result.conflicts);
    let text = String::from_utf8(result.bytes).unwrap();
    assert!(text.ends_with("  keep: 1\r\n  added:\r\n    nested: 2\r\n"), "{text:?}");
    assert!(!text.replace("\r\n", "").contains('\n'));
}

// Base/target/source histories saved by Unity 6.5 through the CLI merge
// matrix (`LocusMergeMatrixApi`): target and source are independent edits of
// the same saved base.
macro_rules! unity65 {
    ($case:literal) => {
        (
            include_bytes!(concat!("fixtures/unity65/", $case, "-base.yaml")).as_slice(),
            include_bytes!(concat!("fixtures/unity65/", $case, "-target.yaml")).as_slice(),
            include_bytes!(concat!("fixtures/unity65/", $case, "-source.yaml")).as_slice(),
        )
    };
}

fn conflicts(session: &MergeSession) -> Vec<&Change> {
    session.catalog().changes.iter().filter(|c| c.status == ChangeStatus::Conflict).collect()
}

#[test]
fn both_sides_adding_components_and_children_to_a_nested_instance_merge() {
    // Both sides add a component and a child GameObject to the same nested
    // prefab instance. Unity lists them in m_AddedComponents/m_AddedGameObjects
    // (empty flow lists in the base) and writes the same stripped GameObject
    // proxy on both sides.
    let (base, target, source) = unity65!("prefab_nested_added");
    let session = prepare_merge(base, target, source).unwrap();
    assert!(conflicts(&session).is_empty(), "{:#?}", conflicts(&session));
    let proxy = session.catalog().changes.iter().find(|c| c.object_id == "8280253604922895859").unwrap();
    assert_eq!(proxy.status, ChangeStatus::AlreadyApplied);
    let added: Vec<_> = session
        .catalog()
        .changes
        .iter()
        .filter(|c| c.property_path.contains("/m_Added"))
        .map(|c| c.property_path.as_str())
        .collect();
    assert!(added.iter().all(|p| p.contains("/@added=")), "{added:?}");
    let output = session.render(&include_all(&session)).unwrap();
    assert!(output.ready, "{:?}", output.conflicts);
    let text = String::from_utf8(output.bytes).unwrap();
    for expected in ["m_Name: TargetLeaf", "m_Name: SourceLeaf", "\nBoxCollider:", "\nAudioSource:"] {
        assert!(text.contains(expected), "missing {expected}");
    }
    assert_eq!(text.matches("&8280253604922895859 stripped").count(), 1);
    let modification = text.split("m_AddedGameObjects:").nth(1).unwrap();
    assert_eq!(modification.split("m_SourcePrefab").next().unwrap().matches("addedObject:").count(), 4);
}

#[test]
fn variant_children_added_on_both_sides_and_a_removed_component_merge() {
    let (base, target, source) = unity65!("prefab_variant");
    let output = rendered(base, target, source);
    assert!(output.ready, "{:?}", output.conflicts);
    let text = String::from_utf8(output.bytes).unwrap();
    assert!(text.contains("m_Name: TargetChild") && text.contains("m_Name: SourceChild"));
    let removed = text.split("m_RemovedComponents:").nth(1).unwrap();
    assert!(removed.trim_start().starts_with("- {fileID:"), "the destination's removed component was lost");
    assert!(validate(&parse(&text.into_bytes()).unwrap()).iter().all(|d| d.severity != Severity::Error));
}

#[test]
fn different_objects_added_with_one_scene_file_id_conflict_with_a_reason() {
    // One Editor session assigns the same next scene fileID to an object added
    // on each side: two different objects share an identity.
    let (base, target, source) = unity65!("scene_same_id");
    let session = prepare_merge(base, target, source).unwrap();
    let open = conflicts(&session);
    assert_eq!(open.len(), 2, "{open:#?}");
    for change in &open {
        assert!(change.property_path.is_empty());
        assert!(change.reason.as_deref().unwrap().contains("both sides added an object with this fileID"));
    }
    // The shared root list itself is identical on both sides.
    assert!(session.catalog().changes.iter().filter(|c| c.property_path.contains("m_Roots")).all(|c| c.status != ChangeStatus::Conflict));
}

fn scene_roots(roots: &[u64]) -> Vec<u8> {
    let mut sorted = roots.to_vec();
    sorted.sort();
    let transforms: String = sorted
        .iter()
        .map(|id| format!("--- !u!4 &{id}\nTransform:\n  m_Children: []\n  m_Father: {{fileID: 0}}\n"))
        .collect();
    let items: String = roots.iter().map(|id| format!("  - {{fileID: {id}}}\n")).collect();
    format!("%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n{transforms}--- !u!1660057539 &9223372036854775807\nSceneRoots:\n  m_ObjectHideFlags: 0\n  m_Roots:\n{items}").into_bytes()
}

#[test]
fn scene_root_lists_merge_by_root_and_keep_their_order() {
    let output = rendered(&scene_roots(&[11, 12]), &scene_roots(&[11, 12, 21]), &scene_roots(&[11, 12, 31]));
    assert!(output.ready, "{:?}", output.conflicts);
    let text = String::from_utf8(output.bytes).unwrap();
    let roots: Vec<&str> = text
        .split("  m_Roots:\n")
        .nth(1)
        .unwrap()
        .lines()
        .take_while(|line| line.starts_with("  - "))
        .collect();
    assert_eq!(roots, ["  - {fileID: 11}", "  - {fileID: 12}", "  - {fileID: 31}", "  - {fileID: 21}"]);
    assert!(text.contains("--- !u!4 &31\n"), "the source root's Transform must be added");
    // Root order is the hierarchy order: a source reordering needs a decision.
    let session = prepare_merge(&scene_roots(&[11, 12]), &scene_roots(&[11, 12, 21]), &scene_roots(&[12, 11])).unwrap();
    let open = conflicts(&session);
    assert_eq!(open.len(), 1);
    assert!(open[0].reason.as_deref().unwrap().contains("reordered"));
}

#[test]
fn identity_less_list_conflicts_name_the_combining_step() {
    let list = |values: &[u32]| {
        let items: String = values.iter().map(|v| format!("  - {v}\n")).collect();
        asset(&format!("  values:\n{items}"))
    };
    let reason = |target: &[u32], source: &[u32]| {
        let session = prepare_merge(&list(&[1, 2]), &list(target), &list(source)).unwrap();
        conflicts(&session)[0].reason.clone().unwrap()
    };
    assert!(reason(&[1, 2, 3], &[1, 2, 4]).contains("fields.set this path to the combined list"));
    assert!(reason(&[5, 2], &[1, 6]).contains("fields.take the other side's changed element paths"));
    assert!(reason(&[2], &[1, 2, 4]).contains("no Unity identity"));
}

#[test]
fn deleted_managed_reference_conflict_names_the_fields_that_point_to_it() {
    let registry = |optional: &str, entries: &[(&str, u32)]| {
        let refs: String = entries
            .iter()
            .map(|(rid, health)| format!("    - rid: {rid}\n      type: {{class: A, ns: N, asm: M}}\n      data:\n        health: {health}\n"))
            .collect();
        asset(&format!("  optional: {{rid: {optional}}}\n  other: {{rid: 8}}\n  references:\n    version: 2\n    RefIds:\n{refs}"))
    };
    let base = registry("7", &[("7", 5), ("8", 1)]);
    let target = registry("-2", &[("8", 1)]);
    let source = registry("7", &[("7", 6), ("8", 1)]);
    let session = prepare_merge(&base, &target, &source).unwrap();
    let open = conflicts(&session);
    assert_eq!(open.len(), 1, "{open:#?}");
    assert!(open[0].property_path.ends_with("/RefIds/@rid=7"));
    let reason = open[0].reason.as_deref().unwrap();
    assert!(reason.contains("pointed to by /MonoBehaviour/optional"), "{reason}");
    // The reverse: the source drops the reference the destination edited.
    let session = prepare_merge(&base, &source, &target).unwrap();
    let reason = conflicts(&session).iter().find(|c| c.property_path.ends_with("@rid=7")).unwrap().reason.clone().unwrap();
    assert!(reason.contains("the source deleted this managed reference") && reason.contains("/MonoBehaviour/optional"), "{reason}");
}

#[test]
fn element_identities_of_unity_lists_agree_for_yaml_and_json() {
    let yaml = concat!(
        "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1001 &5\nPrefabInstance:\n  m_Modification:\n",
        "    m_RemovedComponents:\n    - {fileID: 7, guid: f70e46c013e90204b92504bb4fdd5c29, type: 3}\n",
        "    m_AddedComponents:\n    - targetCorrespondingSourceObject: {fileID: 8, guid: f70e46c013e90204b92504bb4fdd5c29, type: 3}\n",
        "      insertIndex: -1\n      addedObject: {fileID: 1038217950071812644}\n",
    );
    let parsed = parse(yaml.as_bytes()).unwrap();
    let modification = parsed.documents[0].root.get("PrefabInstance").unwrap().get("m_Modification").unwrap();
    let removed = sequence_keys(&parsed, modification.get("m_RemovedComponents").unwrap(), "/PrefabInstance/m_Modification/m_RemovedComponents");
    assert_eq!(removed, Some(vec!["@removed=f70e46c013e90204b92504bb4fdd5c29:7".to_string()]));
    let added = sequence_keys(&parsed, modification.get("m_AddedComponents").unwrap(), "/PrefabInstance/m_Modification/m_AddedComponents");
    assert_eq!(added, Some(vec!["@added=1038217950071812644".to_string()]));
    let json_removed = identity::json_sequence_keys(
        "/PrefabInstance/m_Modification/m_RemovedComponents",
        &[serde_json::json!({"fileID": 7, "guid": "f70e46c013e90204b92504bb4fdd5c29", "type": 3})],
    );
    assert_eq!(json_removed, removed);
    let json_added = identity::json_sequence_keys(
        "/PrefabInstance/m_Modification/m_AddedComponents",
        &[serde_json::json!({"targetCorrespondingSourceObject": {"fileID": 8, "guid": "f70e46c013e90204b92504bb4fdd5c29", "type": 3}, "insertIndex": -1, "addedObject": {"fileID": 1038217950071812644u64}})],
    );
    assert_eq!(json_added, added);
    // Unrelated lists of the same name outside a PrefabInstance stay identity-less.
    assert_eq!(identity::ListIdentity::of("/MonoBehaviour/m_AddedComponents"), None);
    assert_eq!(identity::ListIdentity::of("/MonoBehaviour/m_Roots"), None);
}
