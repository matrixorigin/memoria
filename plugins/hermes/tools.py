"""Provider-owned schemas; identity and branch are deliberately not model arguments."""

import re

TYPES = ["semantic", "profile", "procedural", "working", "episodic", "tool_result"]
SIGNALS = ["useful", "irrelevant", "wrong", "outdated"]


def schema(name, description, properties, required=()):
    return {
        "name": name,
        "description": description,
        "parameters": {
            "type": "object",
            "properties": properties,
            "required": list(required),
            "additionalProperties": False,
        },
    }


TEXT = {"type": "string", "minLength": 1, "maxLength": 24000}
ID = {"type": "string", "minLength": 1, "maxLength": 200}
SCHEMAS = [
    schema(
        "memoria_search",
        "Recall relevant memories for the current user/profile.",
        {"query": TEXT, "top_k": {"type": "integer", "minimum": 1, "maximum": 20}},
        ["query"],
    ),
    schema(
        "memoria_store",
        "Save an explicit fact or preference the user wants remembered.",
        {"content": TEXT, "memory_type": {"type": "string", "enum": TYPES}},
        ["content"],
    ),
    schema(
        "memoria_update",
        "Correct one exact memory ID. Search first to find its ID.",
        {"memory_id": ID, "new_content": TEXT},
        ["memory_id", "new_content"],
    ),
    schema(
        "memoria_forget",
        "Permanently delete one exact memory ID at the user's request.",
        {"memory_id": ID},
        ["memory_id"],
    ),
    schema(
        "memoria_profile",
        "Read a page of the current user's stored profile.",
        {
            "cursor": {"type": "string", "maxLength": 32, "pattern": "[A-Fa-f0-9]{32}"},
            "limit": {"type": "integer", "minimum": 1, "maximum": 50},
        },
    ),
    schema(
        "memoria_feedback",
        "Rate a recalled memory's usefulness or accuracy.",
        {"memory_id": ID, "signal": {"type": "string", "enum": SIGNALS}},
        ["memory_id", "signal"],
    ),
]


def validate(name: str, args: dict):
    spec = next((s["parameters"] for s in SCHEMAS if s["name"] == name), None)
    if spec is None or not isinstance(args, dict):
        raise ValueError("unknown_tool_or_invalid_arguments")
    if set(args) - set(spec["properties"]) or set(spec["required"]) - set(args):
        raise ValueError("invalid_tool_arguments")
    for key, value in args.items():
        prop = spec["properties"][key]
        if prop["type"] == "string":
            if (
                not isinstance(value, str)
                or not value.strip()
                or len(value) > prop.get("maxLength", 24000)
                or ("enum" in prop and value not in prop["enum"])
                or ("pattern" in prop and not re.fullmatch(prop["pattern"], value))
            ):
                raise ValueError(f"invalid_{key}")
            if key in {"content", "new_content"} and len(value.encode("utf-8")) > 32768:
                raise ValueError(f"{key}_exceeds_32_KiB")
        elif type(value) is not int or not prop["minimum"] <= value <= prop["maximum"]:
            raise ValueError(f"invalid_{key}")
