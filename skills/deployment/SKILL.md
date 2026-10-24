---
name: deployment
description: Deploy Memoria with Docker Compose or Kubernetes. Environment variables, multi-instance setup, security. Use when deploying or configuring Memoria.
---

## Docker Compose (Single Instance)

```bash
cd Memoria
cp .env.example .env   # Set MEMORIA_MASTER_KEY, MEMORIA_EMBEDDING_API_KEY
docker compose up -d
```

Services: API on `:8100`, MatrixOne on `:6001`. Verify: `curl http://localhost:8100/health`

## Environment Variables

### Required

| Variable | Description |
|----------|-------------|
| `MEMORIA_MASTER_KEY` | Admin API key (min 16 chars) |

### Database

| Variable | Default | Description |
|----------|---------|-------------|
| `MEMORIA_DB_HOST` | `matrixone` | MatrixOne host |
| `MEMORIA_DB_PORT` | `6001` | MatrixOne port |
| `MEMORIA_DB_USER` | `root` | Database user |
| `MEMORIA_DB_PASSWORD` | `111` | Database password |
| `MEMORIA_DB_NAME` | `memoria` | Database name |

### Embedding

| Variable | Default | Description |
|----------|---------|-------------|
| `MEMORIA_EMBEDDING_PROVIDER` | `local` | `local` or `openai` |
| `MEMORIA_EMBEDDING_MODEL` | `all-MiniLM-L6-v2` | Model name |
| `MEMORIA_EMBEDDING_API_KEY` | — | Required if provider is `openai` (single-backend) |
| `MEMORIA_EMBEDDING_BASE_URL` | — | Custom endpoint, OpenAI-compatible (single-backend) |
| `MEMORIA_EMBEDDING_ENDPOINTS` | — | JSON array for multi-backend round-robin. When set, supersedes `BASE_URL`/`API_KEY`. Format: `[{"url":"https://api1.example.com/v1","api_key":"sk-1"},{"url":"https://api2.example.com/v1","api_key":"sk-2"}]`. All endpoints must serve the same model. Requests rotate round-robin; failed/rate-limited endpoints are skipped automatically. |
| `MEMORIA_EMBEDDING_DIM` | `0` (auto) | Embedding dimension. `0` = auto-infer: Memoria probes the embedding service on startup and uses the returned vector length. Set explicitly (e.g. `768`, `1024`) to skip the probe or when the embedding service may be unavailable at boot time. |

### Distributed

| Variable | Default | Description |
|----------|---------|-------------|
| `MEMORIA_INSTANCE_ID` | Random UUID | Unique instance ID. Set to Pod name in K8s |
| `MEMORIA_LOCK_TTL_SECS` | `120` | Distributed lock TTL. Heartbeat renews every TTL/3 |

### Governance

| Variable | Default | Description |
|----------|---------|-------------|
| `MEMORIA_GOVERNANCE_ENABLED` | `false` | Enable background governance scheduler |
| `MEMORIA_GOVERNANCE_PLUGIN_BINDING` | `default` | Repository binding name |
| `MEMORIA_GOVERNANCE_PLUGIN_SUBJECT` | `system` | Subject key for binding resolution |
| `MEMORIA_GOVERNANCE_PLUGIN_DIR` | — | Local plugin dir (dev mode, skips signature) |

### Security

| Variable | Default | Description |
|----------|---------|-------------|
| `MEMORIA_API_KEY_SECRET` | — | HMAC secret for key hashing. Set independently of MASTER_KEY |

### LLM (for reflect/extract-entities/episodic)

| Variable | Description |
|----------|-------------|
| `LLM_API_KEY` | OpenAI-compatible API key |
| `LLM_BASE_URL` | API base URL |
| `LLM_MODEL` | Model name |

## External MatrixOne

Use an existing MatrixOne instead of the bundled one:

```bash
MEMORIA_DB_HOST=your-host MEMORIA_DB_PORT=6001 docker compose up -d api
```

Tables are auto-created on first startup.

## Multi-Instance (K8s)

All coordination uses the shared MatrixOne database — zero external dependencies.

How it works:
- **Governance scheduler**: Each instance acquires a DB lock before each task. Only one wins; others skip. Heartbeat renews every TTL/3.
- **Async tasks**: Stored in `mem_async_tasks` table, visible cross-instance.
- **Lock expiry**: Crashed instance's locks expire after TTL, another takes over.
- **Single-instance**: `NoopDistributedLock` — zero overhead, identical behavior.

### K8s Manifest

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: memoria-api
spec:
  replicas: 3
  selector:
    matchLabels:
      app: memoria-api
  template:
    metadata:
      labels:
        app: memoria-api
    spec:
      containers:
        - name: memoria
          image: matrixorigin/memoria:latest
          ports:
            - containerPort: 8100
          env:
            - name: MEMORIA_INSTANCE_ID
              valueFrom:
                fieldRef:
                  fieldPath: metadata.name
            - name: MEMORIA_MASTER_KEY
              valueFrom:
                secretKeyRef:
                  name: memoria-secrets
                  key: master-key
            - name: MEMORIA_DB_HOST
              value: "matrixone.default.svc.cluster.local"
            - name: MEMORIA_DB_PORT
              value: "6001"
            - name: MEMORIA_EMBEDDING_PROVIDER
              value: "openai"
            - name: MEMORIA_EMBEDDING_API_KEY
              valueFrom:
                secretKeyRef:
                  name: memoria-secrets
                  key: embedding-api-key
            # Optional: multi-backend round-robin (supersedes API_KEY + MODEL single-backend)
            # - name: MEMORIA_EMBEDDING_ENDPOINTS
            #   value: '[{"url":"https://api1.example.com/v1","api_key":"sk-1"},{"url":"https://api2.example.com/v1","api_key":"sk-2"}]'
            - name: MEMORIA_EMBEDDING_MODEL
              value: "BAAI/bge-m3"
            - name: MEMORIA_EMBEDDING_DIM
              value: "1024"
            - name: MEMORIA_GOVERNANCE_ENABLED
              value: "true"
            - name: MEMORIA_LOCK_TTL_SECS
              value: "120"
          livenessProbe:
            httpGet:
              path: /health
              port: 8100
            initialDelaySeconds: 5
            periodSeconds: 10
          readinessProbe:
            httpGet:
              path: /health/instance
              port: 8100
            initialDelaySeconds: 5
            periodSeconds: 10
---
apiVersion: v1
kind: Service
metadata:
  name: memoria-api
spec:
  selector:
    app: memoria-api
  ports:
    - port: 8100
      targetPort: 8100
```

Key points:
- `MEMORIA_INSTANCE_ID` → `fieldRef: metadata.name` (Pod name)
- All replicas share one MatrixOne
- Leader election is automatic
- Liveness: `/health`, Readiness: `/health/instance`

## Data Persistence

MatrixOne data bind-mounted to `./data/matrixone`. Survives restarts. Change: `MATRIXONE_DATA_DIR=/your/path`

## Security Notes

- API keys are HMAC-SHA256 hashed at rest
- Set `MEMORIA_API_KEY_SECRET` independently of `MASTER_KEY` for key rotation
- All queries scoped to authenticated `user_id`
- Snapshot names are regex-validated before SQL
- Rate limiting per API key (in-memory sliding window)
- Run behind reverse proxy with TLS in production

## Rate Limits

Configurable via env: `MEMORIA_RATE_LIMIT_AUTH_KEYS=1000,60` (format: `max_requests,window_seconds`)
