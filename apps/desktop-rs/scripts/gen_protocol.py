#!/usr/bin/env python3
"""Generate typed Rust serde models from apps/shared/src/gateway-contract.openrpc.json.

Reads the canonical OpenRPC 1.3.2 contract emitted by scripts/gen_gateway_contracts.py
and generates crates/hermes-protocol/src/generated.rs covering:
  - All 54 canonical `hermes-gateway-v1` methods (`x-canonical-methods`)
  - Shared non-canonical methods used by local/desktop clients (capabilities, ping, slash, subagents)
  - All 16 server->client requests (`x-server-requests`)
  - All 79 server->client notification payloads (`x-notifications`)
  - All transitively referenced `#/components/schemas/*` types
"""
from __future__ import annotations

import json
import re
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
OPENRPC_PATH = REPO_ROOT / "apps" / "shared" / "src" / "gateway-contract.openrpc.json"
OUT_PATH = (
    REPO_ROOT
    / "apps"
    / "desktop-rs"
    / "crates"
    / "hermes-protocol"
    / "src"
    / "generated.rs"
)

# Extra non-canonical methods from `methods` needed for Milestone 0 companion UI.
SHARED_METHODS = {
    "client.capabilities",
    "gateway.ping",
    "commands.catalog",
    "complete.slash",
    "complete.path",
    "slash.exec",
    "command.dispatch",
    "subagent.list",
    "subagent.tail",
    "subagent.steer",
    "subagent.interrupt",
    "model.list",
    "model.switch",
}

RUST_KEYWORDS = {
    "as", "async", "await", "break", "const", "continue", "crate", "dyn",
    "else", "enum", "extern", "false", "fn", "for", "if", "impl", "in",
    "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
    "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while", "abstract", "become", "box", "do",
    "final", "macro", "override", "priv", "try", "typeof", "unsized",
    "virtual", "yield",
}


def to_pascal(name: str) -> str:
    parts = re.split(r"[^A-Za-z0-9]+", name)
    out = "".join(p[:1].upper() + p[1:] for p in parts if p)
    if not out or out[0].isdigit():
        out = "V" + out
    return out


def to_snake(name: str) -> str:
    s1 = re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", name)
    s2 = re.sub(r"[^A-Za-z0-9]+", "_", s1).strip("_").lower()
    if not s2 or s2[0].isdigit():
        s2 = f"f_{s2}"
    if s2 in RUST_KEYWORDS:
        return f"r#{s2}"
    return s2


def doc_lines(text: str | None, indent: str = "") -> list[str]:
    if not text:
        return []
    lines = []
    for raw in text.strip().splitlines():
        clean = raw.replace("*/", "* /")
        lines.append(f"{indent}/// {clean}" if clean else f"{indent}///")
    return lines


def ref_name(ref: str) -> str:
    return ref.rsplit("/", 1)[-1]


def collect_refs(node: object, out: set[str]) -> None:
    if isinstance(node, dict):
        if "$ref" in node and isinstance(node["$ref"], str):
            out.add(ref_name(node["$ref"]))
        for v in node.values():
            collect_refs(v, out)
    elif isinstance(node, list):
        for x in node:
            collect_refs(x, out)


def find_recursive_edges(schemas: dict[str, dict], selected: set[str]) -> set[tuple[str, str]]:
    """Return (owner, target) edges that lie on a direct type cycle so we Box<T> them."""
    adj: dict[str, set[str]] = {}
    for name in selected:
        deps: set[str] = set()
        collect_refs(schemas[name], deps)
        adj[name] = deps & selected

    recursive: set[tuple[str, str]] = set()
    # Simple DFS back-edge detection for cycles.
    state: dict[str, int] = {}  # 0=unvisited, 1=visiting, 2=done

    def dfs(u: str) -> None:
        state[u] = 1
        for v in sorted(adj.get(u, ())):
            st = state.get(v, 0)
            if st == 1:
                recursive.add((u, v))
            elif st == 0:
                dfs(v)
        state[u] = 2

    for name in sorted(selected):
        if state.get(name, 0) == 0:
            dfs(name)
    return recursive


def resolve_type(
    schema: dict,
    owner: str,
    selected: set[str],
    recursive_edges: set[tuple[str, str]],
) -> tuple[str, bool]:
    """Return (rust_type, is_nullable)."""
    if not isinstance(schema, dict) or not schema:
        return ("serde_json::Value", False)

    if "$ref" in schema:
        target = ref_name(schema["$ref"])
        if target in selected:
            if (owner, target) in recursive_edges:
                return (f"Box<{target}>", False)
            return (target, False)
        return ("serde_json::Value", False)

    if "anyOf" in schema:
        variants = schema["anyOf"]
        non_null = [v for v in variants if not (isinstance(v, dict) and v.get("type") == "null")]
        has_null = len(non_null) < len(variants)
        if len(non_null) == 1:
            inner_ty, inner_null = resolve_type(non_null[0], owner, selected, recursive_edges)
            return (inner_ty, has_null or inner_null)
        return ("serde_json::Value", has_null)

    if "const" in schema:
        val = schema["const"]
        if isinstance(val, bool):
            return ("bool", False)
        if isinstance(val, int):
            return ("i64", False)
        if isinstance(val, str):
            return ("String", False)
        return ("serde_json::Value", False)

    if "enum" in schema:
        vals = schema["enum"]
        if all(isinstance(v, str) for v in vals):
            return ("String", False)
        if all(isinstance(v, int) and not isinstance(v, bool) for v in vals):
            return ("i64", False)
        return ("serde_json::Value", False)

    ty = schema.get("type")
    if ty == "string":
        return ("String", False)
    if ty == "boolean":
        return ("bool", False)
    if ty == "integer":
        return ("i64", False)
    if ty == "number":
        return ("f64", False)
    if ty == "null":
        return ("()", True)
    if ty == "array":
        items = schema.get("items")
        if isinstance(items, dict):
            item_ty, item_null = resolve_type(items, owner, selected, recursive_edges)
            if item_null:
                return (f"Vec<Option<{item_ty}>>", False)
            return (f"Vec<{item_ty}>", False)
        return ("Vec<serde_json::Value>", False)
    if ty == "object":
        addl = schema.get("additionalProperties")
        if isinstance(addl, dict):
            val_ty, val_null = resolve_type(addl, owner, selected, recursive_edges)
            if val_null:
                return (f"std::collections::BTreeMap<String, Option<{val_ty}>>", False)
            return (f"std::collections::BTreeMap<String, {val_ty}>", False)
        return ("serde_json::Value", False)

    return ("serde_json::Value", False)


def emit_schema(
    name: str,
    schema: dict,
    selected: set[str],
    recursive_edges: set[tuple[str, str]],
    non_default_types: set[str],
) -> list[str]:
    lines: list[str] = []
    lines.extend(doc_lines(schema.get("description")))

    # Top-level string enum
    if "enum" in schema and all(isinstance(v, str) for v in schema["enum"]):
        lines.append("#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]")
        lines.append(f"pub enum {name} {{")
        seen_variants: set[str] = set()
        for idx, raw in enumerate(schema["enum"]):
            vname = to_pascal(raw)
            if vname in seen_variants:
                vname = f"{vname}_{len(seen_variants)}"
            seen_variants.add(vname)
            if idx == 0:
                lines.append("    #[default]")
            lines.append(f'    #[serde(rename = "{raw}")]')
            lines.append(f"    {vname},")
        lines.append("}")
        return lines

    # Top-level untagged union of $refs
    if "anyOf" in schema and "properties" not in schema:
        non_null = [v for v in schema["anyOf"] if not (isinstance(v, dict) and v.get("type") == "null")]
        if len(non_null) > 1 and all(isinstance(v, dict) and "$ref" in v for v in non_null):
            lines.append("#[derive(Debug, Clone, Serialize, Deserialize)]")
            lines.append("#[serde(untagged)]")
            lines.append(f"pub enum {name} {{")
            seen_v: set[str] = set()
            for v in non_null:
                target = ref_name(v["$ref"])
                vname = target
                if vname in seen_v:
                    vname = f"{vname}_{len(seen_v)}"
                seen_v.add(vname)
                ty = f"Box<{target}>" if (name, target) in recursive_edges else target
                lines.append(f"    {vname}({ty}),")
            lines.append("}")
            return lines
        # Fallback alias for mixed anyOf
        lines.append(f"pub type {name} = serde_json::Value;")
        return lines

    # Object with properties -> struct
    if schema.get("type") == "object" or "properties" in schema:
        props = schema.get("properties") or {}
        required = set(schema.get("required") or [])
        allow_extra = schema.get("additionalProperties", True) is not False

        if not props and allow_extra:
            lines.append(f"pub type {name} = serde_json::Map<String, serde_json::Value>;")
            return lines

        can_default = name not in non_default_types
        derives = "Debug, Clone, Default, Serialize, Deserialize" if can_default else "Debug, Clone, Serialize, Deserialize"
        lines.append(f"#[derive({derives})]")
        lines.append(f"pub struct {name} {{")
        used_fields: set[str] = set()
        for raw_key, prop_schema in props.items():
            field_name = to_snake(raw_key)
            if field_name in used_fields:
                field_name = f"{field_name}_{len(used_fields)}"
            used_fields.add(field_name)

            ty, is_nullable = resolve_type(prop_schema, name, selected, recursive_edges)
            is_req = raw_key in required
            # Make non-required or nullable fields Option<T> so partial responses never fail deserialization.
            optional = (not is_req) or is_nullable
            if optional:
                if ty == "()":
                    final_ty = "Option<serde_json::Value>"
                else:
                    final_ty = f"Option<{ty}>"
            else:
                final_ty = ty

            lines.extend(doc_lines(prop_schema.get("description"), indent="    "))
            attrs: list[str] = []
            bare_ident = field_name[2:] if field_name.startswith("r#") else field_name
            if bare_ident != raw_key:
                attrs.append(f'rename = "{raw_key}"')
            if optional:
                attrs.append("default")
                attrs.append('skip_serializing_if = "Option::is_none"')
            elif ty not in non_default_types and not any(ty == f"Box<{nd}>" for nd in non_default_types):
                attrs.append("default")
            if attrs:
                lines.append(f"    #[serde({', '.join(attrs)})]")
            lines.append(f"    pub {field_name}: {final_ty},")

        if allow_extra:
            extra_field = "extra" if "extra" not in used_fields else "extra_fields"
            lines.append("    #[serde(flatten)]")
            lines.append(f"    pub {extra_field}: std::collections::BTreeMap<String, serde_json::Value>,")

        lines.append("}")
        return lines

    # Scalar/array/other alias
    ty, _ = resolve_type(schema, name, selected, recursive_edges)
    lines.append(f"pub type {name} = {ty};")
    return lines


def method_param_schema(entry: dict) -> str:
    params = entry.get("params") or []
    if params and isinstance(params[0], dict):
        s = params[0].get("schema") or {}
        if "$ref" in s:
            return ref_name(s["$ref"])
    return "serde_json::Value"


def method_result_schema(entry: dict) -> str:
    res = entry.get("result") or {}
    s = res.get("schema") or {}
    if "$ref" in s:
        return ref_name(s["$ref"])
    return "serde_json::Value"


def main() -> None:
    doc = json.loads(OPENRPC_PATH.read_text(encoding="utf-8"))
    all_schemas: dict[str, dict] = doc["components"]["schemas"]

    canonical_methods: list[dict] = doc.get("x-canonical-methods", [])
    shared_methods: list[dict] = [
        m for m in doc.get("methods", []) if m["name"] in SHARED_METHODS
    ]
    server_requests: list[dict] = doc.get("x-server-requests", [])
    notifications: list[dict] = doc.get("x-notifications", [])

    # Collect transitive closure of all referenced schemas.
    roots: set[str] = set()
    for group in (canonical_methods, shared_methods, server_requests, notifications):
        collect_refs(group, roots)

    selected: set[str] = set()
    queue = list(roots)
    while queue:
        cur = queue.pop()
        if cur in selected or cur not in all_schemas:
            continue
        selected.add(cur)
        deps: set[str] = set()
        collect_refs(all_schemas[cur], deps)
        for d in deps:
            if d not in selected:
                queue.append(d)

    recursive_edges = find_recursive_edges(all_schemas, selected)

    # Identify schemas that cannot derive Default (untagged union enums and structs requiring them).
    non_default_types: set[str] = set()
    for name in selected:
        sc = all_schemas[name]
        if "anyOf" in sc and "properties" not in sc:
            nn = [v for v in sc["anyOf"] if not (isinstance(v, dict) and v.get("type") == "null")]
            if len(nn) > 1 and all(isinstance(v, dict) and "$ref" in v for v in nn):
                non_default_types.add(name)
    changed = True
    while changed:
        changed = False
        for name in selected:
            if name in non_default_types:
                continue
            sc = all_schemas[name]
            props = sc.get("properties") or {}
            req = set(sc.get("required") or [])
            for rk, ps in props.items():
                if rk not in req:
                    continue
                ty, is_null = resolve_type(ps, name, selected, recursive_edges)
                if not is_null and (ty in non_default_types or any(ty == f"Box<{nd}>" for nd in non_default_types)):
                    non_default_types.add(name)
                    changed = True
                    break

    out: list[str] = [
        "// @generated by apps/desktop-rs/scripts/gen_protocol.py",
        "// Source: apps/shared/src/gateway-contract.openrpc.json",
        "// Do not edit by hand; re-run `python3 apps/desktop-rs/scripts/gen_protocol.py`.",
        "",
        "#![allow(dead_code, clippy::all)]",
        "",
        "use serde::{Deserialize, Serialize};",
        "",
        "/// Trait implemented by every generated JSON-RPC method marker type.",
        "pub trait RpcMethod {",
        "    const NAME: &'static str;",
        "    type Params: Serialize;",
        "    type Result: serde::de::DeserializeOwned;",
        "}",
        "",
        "// ── Schema types ──────────────────────────────────────────────────────────────",
        "",
    ]

    for name in sorted(selected):
        out.extend(emit_schema(name, all_schemas[name], selected, recursive_edges, non_default_types))
        out.append("")

    # Emit typed method markers for canonical + shared methods.
    out.append("// ── Canonical (`hermes-gateway-v1`) & shared RPC methods ───────────────────────")
    out.append("pub mod methods {")
    out.append("    use super::RpcMethod;")
    out.append("")

    seen_method_names: set[str] = set()
    combined_methods = [(m, True) for m in canonical_methods] + [
        (m, False) for m in shared_methods if m["name"] not in {c["name"] for c in canonical_methods}
    ]
    for entry, is_canonical in sorted(combined_methods, key=lambda pair: pair[0]["name"]):
        mname = entry["name"]
        struct_name = to_pascal(mname)
        if struct_name in seen_method_names:
            continue
        seen_method_names.add(struct_name)
        p_raw = method_param_schema(entry)
        r_raw = method_result_schema(entry)
        p_ty = p_raw if p_raw == "serde_json::Value" else f"super::{p_raw}"
        r_ty = r_raw if r_raw == "serde_json::Value" else f"super::{r_raw}"
        tag = "canonical" if is_canonical else "shared"
        summary = entry.get("summary") or f"{mname} ({tag})"
        out.extend(doc_lines(f"`{mname}` ({tag}): {summary}", indent="    "))
        out.append(f"    pub struct {struct_name};")
        out.append(f"    impl RpcMethod for {struct_name} {{")
        out.append(f'        const NAME: &\'static str = "{mname}";')
        out.append(f"        type Params = {p_ty};")
        out.append(f"        type Result = {r_ty};")
        out.append("    }")
        out.append("")
    out.append("}")
    out.append("")

    # Emit typed server->client request enum
    out.append("// ── Server -> client requests ───────────────────────────────────────────────────")
    out.append("#[derive(Debug, Clone)]")
    out.append("pub enum ServerRequestPayload {")
    for entry in sorted(server_requests, key=lambda e: e["name"]):
        vname = to_pascal(entry["name"])
        p_ty = method_param_schema(entry)
        out.extend(doc_lines(entry.get("summary"), indent="    "))
        out.append(f"    {vname}({p_ty}),")
    out.append("    Unknown { method: String, params: serde_json::Value },")
    out.append("}")
    out.append("")
    out.append("impl ServerRequestPayload {")
    out.append("    pub fn parse(method: &str, params: serde_json::Value) -> Result<Self, serde_json::Error> {")
    out.append("        match method {")
    for entry in sorted(server_requests, key=lambda e: e["name"]):
        vname = to_pascal(entry["name"])
        out.append(f'            "{entry["name"]}" => Ok(Self::{vname}(serde_json::from_value(params)?)),')
    out.append("            other => Ok(Self::Unknown { method: other.to_string(), params }),")
    out.append("        }")
    out.append("    }")
    out.append("}")
    out.append("")

    # Emit typed notification enum
    out.append("// ── Server -> client notifications ──────────────────────────────────────────────")
    out.append("#[derive(Debug, Clone)]")
    out.append("pub enum GatewayNotification {")
    for entry in sorted(notifications, key=lambda e: e["name"]):
        vname = to_pascal(entry["name"])
        p_ty = method_param_schema(entry)
        out.extend(doc_lines(entry.get("summary"), indent="    "))
        out.append(f"    {vname}({p_ty}),")
    out.append("    Unknown { event_type: String, payload: serde_json::Value },")
    out.append("}")
    out.append("")
    out.append("impl GatewayNotification {")
    out.append("    pub fn parse(event_type: &str, payload: serde_json::Value) -> Result<Self, serde_json::Error> {")
    out.append("        match event_type {")
    for entry in sorted(notifications, key=lambda e: e["name"]):
        vname = to_pascal(entry["name"])
        out.append(f'            "{entry["name"]}" => Ok(Self::{vname}(serde_json::from_value(payload)?)),')
    out.append("            other => Ok(Self::Unknown { event_type: other.to_string(), payload }),")
    out.append("        }")
    out.append("    }")
    out.append("}")
    out.append("")

    rendered = "\n".join(out)
    if "--check" in __import__("sys").argv[1:]:
        existing = OUT_PATH.read_text(encoding="utf-8") if OUT_PATH.exists() else ""
        if existing != rendered:
            raise SystemExit(
                f"{OUT_PATH.relative_to(REPO_ROOT)} is stale; run `python3 apps/desktop-rs/scripts/gen_protocol.py`"
            )
        print(f"OK: {OUT_PATH.relative_to(REPO_ROOT)} is up to date")
        return

    OUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    OUT_PATH.write_text(rendered, encoding="utf-8")
    print(
        f"Generated {OUT_PATH.relative_to(REPO_ROOT)}: "
        f"{len(selected)} schemas, {len(seen_method_names)} methods, "
        f"{len(server_requests)} server requests, {len(notifications)} notifications"
    )


if __name__ == "__main__":
    main()
