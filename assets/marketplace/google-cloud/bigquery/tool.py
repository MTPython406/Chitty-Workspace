"""
Google BigQuery tool for Chitty Workspace.
Manages datasets, tables, and queries using the user's gcloud CLI credentials.
"""

import json
import re
import sys
import os
import subprocess

# Add parent dir so we can import shared helpers
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

try:
    from chitty_sdk import require_credential, check_feature
except ImportError:
    pass  # Fall back to local helpers

from auth import get_access_token, get_project_id
from config import check_resource_allowed, check_feature_allowed, get_allowed_resources


# ── Security: input validation helpers ────────────────────────────

# Maximum rows returned from any query
MAX_QUERY_RESULTS = 1000

# SQL keywords that indicate mutating operations
MUTATING_KEYWORDS = re.compile(
    r'\b(DELETE|DROP|CREATE|INSERT|UPDATE|ALTER|TRUNCATE|MERGE|CALL)\b',
    re.IGNORECASE,
)

# Valid BigQuery identifier: alphanumeric + underscore, 1-1024 chars
VALID_IDENTIFIER = re.compile(r'^[A-Za-z0-9_]{1,1024}$')

# Pattern to extract dataset references from SQL (project.dataset.table or dataset.table)
TABLE_REF_PATTERN = re.compile(
    r'`?([A-Za-z0-9_-]+)\.([A-Za-z0-9_]+)\.([A-Za-z0-9_]+)`?'  # project.dataset.table
    r'|'
    r'`?([A-Za-z0-9_]+)\.([A-Za-z0-9_]+)`?'  # dataset.table
)


def validate_query_permissions(sql):
    """Block mutating SQL unless allow_mutating_queries feature flag is enabled.
    Returns (allowed, error_message).
    """
    if MUTATING_KEYWORDS.search(sql):
        allowed, err = check_feature_allowed("allow_mutating_queries", "mutating SQL queries")
        if not allowed:
            return False, (
                "This SQL contains mutating operations (DELETE, DROP, CREATE, INSERT, UPDATE, ALTER, TRUNCATE). "
                "Mutating queries are disabled by default for safety. "
                "Enable 'allow_mutating_queries' in Settings > Marketplace > Google Cloud Platform > Feature Flags."
            )
    return True, None


def validate_dataset_id(dataset_id):
    """Validate a BigQuery dataset identifier.
    Must be alphanumeric + underscore, no path traversal.
    Returns (valid, error_message).
    """
    if not dataset_id:
        return False, "dataset_id is required"
    if not VALID_IDENTIFIER.match(dataset_id):
        return False, (
            f"Invalid dataset_id '{dataset_id}'. "
            "Must contain only letters, numbers, and underscores (1-1024 chars)."
        )
    if '..' in dataset_id or '/' in dataset_id or '\\' in dataset_id:
        return False, f"Invalid dataset_id '{dataset_id}': path traversal not allowed."
    return True, None


def validate_table_id(table_id):
    """Validate a BigQuery table identifier.
    Must be alphanumeric + underscore, no path traversal.
    Returns (valid, error_message).
    """
    if not table_id:
        return False, "table_id is required"
    if not VALID_IDENTIFIER.match(table_id):
        return False, (
            f"Invalid table_id '{table_id}'. "
            "Must contain only letters, numbers, and underscores (1-1024 chars)."
        )
    if '..' in table_id or '/' in table_id or '\\' in table_id:
        return False, f"Invalid table_id '{table_id}': path traversal not allowed."
    return True, None


def normalize_max_results(params):
    """Clamp max_results to MAX_QUERY_RESULTS (1000)."""
    raw = params.get("max_results")
    if raw is not None:
        try:
            val = int(raw)
            return max(1, min(val, MAX_QUERY_RESULTS))
        except (ValueError, TypeError):
            return 100  # default
    return 100  # default


def extract_query_datasets(sql):
    """Extract dataset names referenced in SQL table references.
    Parses `project.dataset.table` and `dataset.table` patterns.
    Returns a set of dataset names.
    """
    datasets = set()
    for match in TABLE_REF_PATTERN.finditer(sql):
        # Group 2 = dataset from project.dataset.table
        if match.group(2):
            datasets.add(match.group(2))
        # Group 4 = dataset from dataset.table
        if match.group(4):
            datasets.add(match.group(4))
    return datasets


def enforce_query_datasets(sql):
    """Check that all datasets referenced in SQL are in the allowed list.
    Returns (allowed, error_message).
    If no dataset allow-list is configured, allows all.
    """
    allowed_datasets = get_allowed_resources("datasets")
    if not allowed_datasets:
        return True, None  # No restrictions configured

    referenced = extract_query_datasets(sql)
    if not referenced:
        return True, None  # Could not parse any refs — let BigQuery enforce

    disallowed = referenced - set(allowed_datasets)
    if disallowed:
        return False, (
            f"Query references dataset(s) not in your allowed list: {', '.join(sorted(disallowed))}. "
            f"Allowed datasets: {', '.join(allowed_datasets)}. "
            f"Configure allowed datasets in Settings > Marketplace > Google Cloud Platform."
        )
    return True, None


# ── HTTP helper ───────────────────────────────────────────────────

def make_request(method, url, headers, body=None):
    """Make HTTP request to BigQuery REST API using urllib (no external deps needed)."""
    import urllib.request
    import urllib.error

    data = json.dumps(body).encode() if body else None
    req = urllib.request.Request(url, data=data, headers=headers, method=method)

    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return json.loads(resp.read().decode()), resp.status
    except urllib.error.HTTPError as e:
        error_body = e.read().decode() if e.fp else str(e)
        try:
            error_json = json.loads(error_body)
            msg = error_json.get("error", {}).get("message", error_body)
        except Exception:
            msg = error_body
        return {"error": msg}, e.code


BASE_URL = "https://bigquery.googleapis.com/bigquery/v2"


def resolve_project(params):
    """Get project ID from params or gcloud default."""
    project = params.get("project_id")
    if project:
        return project, None
    project, err = get_project_id()
    if not project:
        return None, err or "No project_id provided and no default gcloud project set"
    return project, None


# ── Actions ───────────────────────────────────────────────────────

def list_datasets(params, headers):
    project, err = resolve_project(params)
    if err:
        return {"success": False, "error": err}

    data, status = make_request("GET", f"{BASE_URL}/projects/{project}/datasets", headers)
    if status != 200:
        return {"success": False, "error": data.get("error", f"HTTP {status}")}

    datasets = []
    for ds in data.get("datasets", []):
        ref = ds.get("datasetReference", {})
        datasets.append({
            "id": ref.get("datasetId"),
            "project": ref.get("projectId"),
            "location": ds.get("location"),
        })
    return {"success": True, "output": {"datasets": datasets, "count": len(datasets)}}


def create_dataset(params, headers):
    dataset_id = params.get("dataset_id")
    valid, err = validate_dataset_id(dataset_id)
    if not valid:
        return {"success": False, "error": err}

    project, err = resolve_project(params)
    if err:
        return {"success": False, "error": err}

    location = params.get("location", "US")
    body = {
        "datasetReference": {
            "datasetId": dataset_id,
            "projectId": project,
        },
        "location": location,
    }

    data, status = make_request("POST", f"{BASE_URL}/projects/{project}/datasets", headers, body)
    if status not in (200, 201):
        return {"success": False, "error": data.get("error", f"HTTP {status}")}

    return {"success": True, "output": f"Dataset '{dataset_id}' created in project '{project}' (location: {location})"}


def delete_dataset(params, headers):
    dataset_id = params.get("dataset_id")
    valid, err = validate_dataset_id(dataset_id)
    if not valid:
        return {"success": False, "error": err}

    project, err = resolve_project(params)
    if err:
        return {"success": False, "error": err}

    data, status = make_request(
        "DELETE",
        f"{BASE_URL}/projects/{project}/datasets/{dataset_id}?deleteContents=true",
        headers
    )
    if status not in (200, 204):
        return {"success": False, "error": data.get("error", f"HTTP {status}")}

    return {"success": True, "output": f"Dataset '{dataset_id}' deleted"}


def list_tables(params, headers):
    dataset_id = params.get("dataset_id")
    valid, err = validate_dataset_id(dataset_id)
    if not valid:
        return {"success": False, "error": err}

    project, err = resolve_project(params)
    if err:
        return {"success": False, "error": err}

    data, status = make_request(
        "GET", f"{BASE_URL}/projects/{project}/datasets/{dataset_id}/tables", headers
    )
    if status != 200:
        return {"success": False, "error": data.get("error", f"HTTP {status}")}

    tables = []
    for t in data.get("tables", []):
        ref = t.get("tableReference", {})
        tables.append({
            "id": ref.get("tableId"),
            "type": t.get("type"),
            "row_count": t.get("numRows"),
        })
    return {"success": True, "output": {"tables": tables, "count": len(tables)}}


def describe_table(params, headers):
    dataset_id = params.get("dataset_id")
    table_id = params.get("table_id")

    valid, err = validate_dataset_id(dataset_id)
    if not valid:
        return {"success": False, "error": err}
    valid, err = validate_table_id(table_id)
    if not valid:
        return {"success": False, "error": err}

    project, err = resolve_project(params)
    if err:
        return {"success": False, "error": err}

    data, status = make_request(
        "GET",
        f"{BASE_URL}/projects/{project}/datasets/{dataset_id}/tables/{table_id}",
        headers
    )
    if status != 200:
        return {"success": False, "error": data.get("error", f"HTTP {status}")}

    schema = data.get("schema", {}).get("fields", [])
    fields = [{"name": f["name"], "type": f["type"], "mode": f.get("mode", "NULLABLE")} for f in schema]
    return {
        "success": True,
        "output": {
            "table_id": table_id,
            "dataset_id": dataset_id,
            "row_count": data.get("numRows"),
            "size_bytes": data.get("numBytes"),
            "fields": fields,
        }
    }


def run_query(params, headers):
    sql = params.get("sql")
    if not sql:
        return {"success": False, "error": "sql is required"}

    # Security: block mutating SQL unless feature flag enabled
    allowed, err = validate_query_permissions(sql)
    if not allowed:
        return {"success": False, "error": err}

    # Security: enforce dataset allow-list on SQL references
    allowed, err = enforce_query_datasets(sql)
    if not allowed:
        return {"success": False, "error": err}

    project, err = resolve_project(params)
    if err:
        return {"success": False, "error": err}

    # Clamp maxResults
    max_results = normalize_max_results(params)

    body = {
        "query": sql,
        "useLegacySql": False,
        "maxResults": max_results,
    }

    data, status = make_request(
        "POST", f"{BASE_URL}/projects/{project}/queries", headers, body
    )
    if status != 200:
        return {"success": False, "error": data.get("error", f"HTTP {status}")}

    # Parse results
    schema = data.get("schema", {}).get("fields", [])
    col_names = [f["name"] for f in schema]

    rows = []
    for row in data.get("rows", []):
        values = row.get("f", [])
        row_dict = {}
        for i, val in enumerate(values):
            if i < len(col_names):
                row_dict[col_names[i]] = val.get("v")
        rows.append(row_dict)

    return {
        "success": True,
        "output": {
            "columns": col_names,
            "rows": rows,
            "total_rows": data.get("totalRows"),
            "job_complete": data.get("jobComplete"),
            "max_results_applied": max_results,
        }
    }


def insert_rows(params, headers):
    dataset_id = params.get("dataset_id")
    table_id = params.get("table_id")
    rows = params.get("rows")

    valid, err = validate_dataset_id(dataset_id)
    if not valid:
        return {"success": False, "error": err}
    valid, err = validate_table_id(table_id)
    if not valid:
        return {"success": False, "error": err}

    if not rows or not isinstance(rows, list):
        return {"success": False, "error": "rows must be a non-empty JSON array"}

    project, err = resolve_project(params)
    if err:
        return {"success": False, "error": err}

    body = {
        "rows": [{"json": row} for row in rows],
    }

    data, status = make_request(
        "POST",
        f"{BASE_URL}/projects/{project}/datasets/{dataset_id}/tables/{table_id}/insertAll",
        headers,
        body
    )
    if status != 200:
        return {"success": False, "error": data.get("error", f"HTTP {status}")}

    errors = data.get("insertErrors", [])
    if errors:
        return {"success": False, "error": f"Insert errors: {json.dumps(errors[:3])}"}

    return {"success": True, "output": f"Inserted {len(rows)} rows into {dataset_id}.{table_id}"}


# ── Main entry point ──────────────────────────────────────────────

ACTIONS = {
    "list_datasets": list_datasets,
    "create_dataset": create_dataset,
    "delete_dataset": delete_dataset,
    "list_tables": list_tables,
    "describe_table": describe_table,
    "query": run_query,
    "insert_rows": insert_rows,
}


def main():
    try:
        raw = sys.stdin.read()
        params = json.loads(raw) if raw.strip() else {}
    except json.JSONDecodeError as e:
        print(json.dumps({"success": False, "error": f"Invalid JSON input: {e}"}))
        sys.exit(0)

    action = params.get("action", "")
    if action not in ACTIONS:
        print(json.dumps({
            "success": False,
            "error": f"Unknown action '{action}'. Available: {', '.join(ACTIONS.keys())}"
        }))
        sys.exit(0)

    # ── Config enforcement: feature flags ────────────────────────
    feature_gates = {
        "create_dataset": "allow_create_dataset",
        "delete_dataset": "allow_delete_dataset",
        "insert_rows": "allow_mutating_queries",
    }
    if action in feature_gates:
        allowed, err = check_feature_allowed(feature_gates[action], action)
        if not allowed:
            print(json.dumps({"success": False, "error": err}))
            sys.exit(0)

    # ── Config enforcement: allowed datasets ─────────────────────
    dataset_actions = ["list_tables", "describe_table", "query", "insert_rows"]
    if action in dataset_actions:
        dataset_id = params.get("dataset_id", "")
        if dataset_id:
            # Validate identifier format first
            valid, err = validate_dataset_id(dataset_id)
            if not valid:
                print(json.dumps({"success": False, "error": err}))
                sys.exit(0)
            allowed, err = check_resource_allowed("datasets", dataset_id)
            if not allowed:
                print(json.dumps({"success": False, "error": err}))
                sys.exit(0)

    # Get auth token
    token, auth_err = get_access_token()
    if not token:
        print(json.dumps({
            "success": False,
            "error": f"Not authenticated with Google Cloud. Run 'gcloud auth login' first. ({auth_err})"
        }))
        sys.exit(0)

    headers = {
        "Authorization": f"Bearer {token}",
        "Content-Type": "application/json",
    }

    result = ACTIONS[action](params, headers)
    print(json.dumps(result))


if __name__ == "__main__":
    main()
