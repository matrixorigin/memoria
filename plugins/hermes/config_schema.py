"""Pure-data schema loaded independently by Hermes Desktop."""

from plugins.memory.config_schema import (
    KIND_BOOL,
    KIND_SECRET,
    ProviderConfigSchema,
    ProviderField,
)

CONFIG_SCHEMA = ProviderConfigSchema(
    name="memoria",
    label="Memoria",
    docs_url="https://thememoria.ai",
    fields=(
        ProviderField(
            key="api_key",
            label="Memoria API Key",
            kind=KIND_SECRET,
            env_key="MEMORIA_API_KEY",
            inline=True,
        ),
        ProviderField(key="api_url", label="API origin", default="https://api.thememoria.ai"),
        ProviderField(
            key="auto_recall",
            label="Recall relevant memories",
            kind=KIND_BOOL,
            default="true",
            inline=True,
        ),
        ProviderField(
            key="auto_capture",
            label="Upload completed user/assistant turns",
            kind=KIND_BOOL,
            default="false",
            inline=True,
            description="Opt in to background fact extraction; excludes tool results.",
        ),
    ),
)
