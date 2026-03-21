---
name: google-cloud
description: >
  Query BigQuery datasets and manage Google Cloud Storage buckets.
  Use when the user asks about BigQuery, SQL queries on cloud data,
  GCS buckets, or Google Cloud Platform resources.
allowed-tools: bigquery cloud_storage terminal
---

# Google Cloud Platform

## Approach

Always check the user's allowed datasets and buckets before performing operations.
Prefer read-only operations unless the user explicitly asks to modify data.

## BigQuery

- List datasets before querying to confirm access
- Use standard SQL syntax
- Limit result sets by default (add LIMIT if user doesn't specify)
- Format query results as markdown tables for readability
- For large results, summarize and offer to export

## Cloud Storage

- List bucket contents before reading/writing
- Confirm before uploading or deleting objects
- Use gsutil commands via terminal for bulk operations

## Gotchas

- Auth uses `gcloud` CLI — ensure the user has run `gcloud auth login`
- BigQuery queries may incur costs — mention this for large scans
- Resource scoping: only operate on datasets/buckets the user has allowed in package config
- Windows: use `gcloud` directly, not `gsutil` (which may not be in PATH)
