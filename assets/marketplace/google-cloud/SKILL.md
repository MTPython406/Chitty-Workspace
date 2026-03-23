---
name: google-cloud
description: >
  Query BigQuery datasets and manage Google Cloud Storage buckets.
  Use when the user asks about BigQuery, SQL queries on cloud data,
  GCS buckets, or Google Cloud Platform resources.
allowed-tools: gcloud_bigquery gcloud_storage execute_package_tool terminal
---

# Google Cloud Platform

## Critical Rules

1. **Always use the package tools** (`gcloud_bigquery`, `gcloud_storage`) via `execute_package_tool` for ALL BigQuery and Cloud Storage operations. These tools use the REST API with gcloud auth tokens — they do NOT need `bq` or `gsutil` in PATH.
2. **NEVER use the browser** to navigate Google Cloud Console. All operations must go through package tools or terminal CLI.
3. **NEVER use `bq` CLI** through the terminal tool. The `gcloud_bigquery` package tool handles everything via REST API.
4. **Only use terminal** for: checking `gcloud` auth status, setting project config, or operations not covered by the package tools.

## BigQuery Operations (via gcloud_bigquery tool)

Use `execute_package_tool` with `gcloud_bigquery` and these actions:

- `list_datasets` — List all datasets in the project
- `list_tables` — List tables in a dataset (requires dataset_id)
- `describe_table` — Get table schema (requires dataset_id + table_id)
- `query` — Run SQL (requires sql). Use for both SELECT and CREATE TABLE/INSERT when mutating is enabled.
- `sample` — Get sample rows from a table (requires dataset_id + table_id). Use FIRST to understand data shape before writing complex queries.
- `insert_rows` — Insert rows (requires dataset_id + table_id + rows array)
- `create_dataset` — Create a new dataset (requires dataset_id)

### Example tool call:
```
execute_package_tool(tool_name="gcloud_bigquery", arguments={"action": "query", "sql": "SELECT * FROM dataset.table LIMIT 10"})
```

## Query Best Practices

- **Sample first**: Before writing complex queries, use the `sample` action to understand column types and data shape
- **Prefer aggregations**: Use COUNT, SUM, AVG, GROUP BY for metrics. Only return full records when the user explicitly asks.
- **Always add LIMIT**: Default to LIMIT 100 unless the user specifies otherwise
- **Format results**: Present query results as markdown tables for readability
- **Cost awareness**: Mention that large scans may incur costs on BigQuery

## Approval & Confirmation

- **Ask before creating, deleting, or modifying** data or resources
- **Once the user approves a task**, execute all steps without re-asking for each sub-step
- Example: If user says "create WMS tables with sample data", ask once for confirmation, then create all 7 tables and insert data without asking again for each table
- **If something fails**, explain the issue and suggest alternatives — don't retry the same approach repeatedly

## Cloud Storage (via gcloud_storage tool)

- List bucket contents before reading/writing
- Confirm before uploading or deleting objects
- For bulk operations, use the terminal with `gsutil` only if the package tool doesn't support the operation

## gcloud CLI via Terminal

If you need to use terminal for gcloud commands on Windows, use the full path:
```
& 'C:\Program Files (x86)\Google\Cloud SDK\google-cloud-sdk\bin\gcloud.cmd' [command]
```

Common locations:
- Windows: `C:\Program Files (x86)\Google\Cloud SDK\google-cloud-sdk\bin\gcloud.cmd`
- Mac: `/usr/local/bin/gcloud` or `~/google-cloud-sdk/bin/gcloud`
- Linux: `/usr/bin/gcloud` or `/snap/bin/gcloud`

## Security

- Auth uses gcloud CLI credentials — no keys stored by Chitty
- Resource scoping: only operate on datasets/buckets the user has allowed in package config
- Feature flags control mutating operations — check before attempting CREATE/INSERT/DELETE
