"""
Google Cloud Storage tool for Chitty Workspace.
Manages buckets and objects using the user's gcloud CLI credentials.
"""

import json
import re
import sys
import os
import tempfile

# Add parent dir so we can import shared helpers
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

try:
    from chitty_sdk import require_credential, check_feature
except ImportError:
    pass  # Fall back to local helpers

from auth import get_access_token, get_project_id
from config import check_resource_allowed, check_feature_allowed


# ── Security: input validation helpers ────────────────────────────

# GCS bucket naming rules: lowercase, alphanumeric, hyphens, dots, 3-63 chars
VALID_BUCKET_NAME = re.compile(r'^[a-z0-9][a-z0-9._-]{1,61}[a-z0-9]$')

# Maximum results for list operations
MAX_LIST_RESULTS = 500

# Maximum upload size: 50 MB
MAX_UPLOAD_BYTES = 50 * 1024 * 1024

# Maximum download size: 50 MB
MAX_DOWNLOAD_BYTES = 50 * 1024 * 1024


def validate_bucket_name(bucket):
    """Validate a GCS bucket name per Google Cloud rules.
    Lowercase, alphanumeric + hyphens + dots, 3-63 chars.
    Returns (valid, error_message).
    """
    if not bucket:
        return False, "bucket name is required"
    if len(bucket) < 3 or len(bucket) > 63:
        return False, f"Invalid bucket name '{bucket}': must be 3-63 characters."
    if not VALID_BUCKET_NAME.match(bucket):
        return False, (
            f"Invalid bucket name '{bucket}': must be lowercase, start/end with alphanumeric, "
            "and contain only lowercase letters, numbers, hyphens, underscores, and dots."
        )
    if '..' in bucket:
        return False, f"Invalid bucket name '{bucket}': consecutive dots not allowed."
    return True, None


def validate_object_path(object_path):
    """Validate a GCS object path, blocking path traversal.
    Returns (valid, error_message).
    """
    if not object_path:
        return False, "object_path is required"
    if '..' in object_path:
        return False, f"Invalid object_path '{object_path}': '..' path traversal not allowed."
    if object_path.startswith('/'):
        return False, f"Invalid object_path '{object_path}': absolute paths not allowed (remove leading '/')."
    if '\\' in object_path:
        return False, f"Invalid object_path '{object_path}': backslashes not allowed."
    if len(object_path) > 1024:
        return False, f"Invalid object_path: exceeds maximum length of 1024 characters."
    return True, None


def resolve_safe_local_path(local_path):
    """Resolve a local file path, confining it to workspace dir or system temp.
    Returns (safe_path, error_message).
    """
    if not local_path:
        return None, "local_path is required"

    # Resolve to absolute, normalizing any ../ etc.
    resolved = os.path.realpath(os.path.abspath(local_path))

    # Allowed base directories
    allowed_bases = []

    # Current working directory (workspace)
    cwd = os.path.realpath(os.getcwd())
    allowed_bases.append(cwd)

    # System temp directory
    tmp = os.path.realpath(tempfile.gettempdir())
    allowed_bases.append(tmp)

    # CHITTY_WORKSPACE_DIR if set
    workspace = os.environ.get("CHITTY_WORKSPACE_DIR")
    if workspace:
        allowed_bases.append(os.path.realpath(workspace))

    # Check if resolved path is under any allowed base
    for base in allowed_bases:
        # Ensure trailing separator for prefix check
        base_prefix = base if base.endswith(os.sep) else base + os.sep
        if resolved == base or resolved.startswith(base_prefix):
            return resolved, None

    return None, (
        f"Local path '{local_path}' is outside allowed directories. "
        f"File operations are restricted to the workspace directory and system temp. "
        f"Resolved path: {resolved}"
    )


def normalize_list_results(params):
    """Clamp max_results for list operations to MAX_LIST_RESULTS."""
    raw = params.get("max_results")
    if raw is not None:
        try:
            val = int(raw)
            return max(1, min(val, MAX_LIST_RESULTS))
        except (ValueError, TypeError):
            return 100
    return 100


# ── HTTP helper ───────────────────────────────────────────────────

def make_request(method, url, headers, body=None, raw_body=None):
    """Make HTTP request to GCS REST API."""
    import urllib.request
    import urllib.error

    if raw_body is not None:
        data = raw_body if isinstance(raw_body, bytes) else raw_body.encode("utf-8")
    elif body is not None:
        data = json.dumps(body).encode()
    else:
        data = None

    req = urllib.request.Request(url, data=data, headers=headers, method=method)

    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            content_type = resp.headers.get("Content-Type", "")
            raw = resp.read()
            if "application/json" in content_type:
                return json.loads(raw.decode()), resp.status
            return raw, resp.status
    except urllib.error.HTTPError as e:
        error_body = e.read().decode() if e.fp else str(e)
        try:
            error_json = json.loads(error_body)
            msg = error_json.get("error", {}).get("message", error_body)
        except Exception:
            msg = error_body
        return {"error": msg}, e.code


STORAGE_API = "https://storage.googleapis.com/storage/v1"
UPLOAD_API = "https://storage.googleapis.com/upload/storage/v1"


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

def list_buckets(params, headers):
    project, err = resolve_project(params)
    if err:
        return {"success": False, "error": err}

    max_results = normalize_list_results(params)
    url = f"{STORAGE_API}/b?project={project}&maxResults={max_results}"
    data, status = make_request("GET", url, headers)

    if status != 200:
        return {"success": False, "error": data.get("error", f"HTTP {status}") if isinstance(data, dict) else f"HTTP {status}"}

    buckets = []
    for b in data.get("items", []):
        buckets.append({
            "name": b.get("name"),
            "location": b.get("location"),
            "storage_class": b.get("storageClass"),
            "created": b.get("timeCreated"),
        })
    return {"success": True, "output": {"buckets": buckets, "count": len(buckets)}}


def create_bucket(params, headers):
    bucket = params.get("bucket")
    valid, err = validate_bucket_name(bucket)
    if not valid:
        return {"success": False, "error": err}

    project, err = resolve_project(params)
    if err:
        return {"success": False, "error": err}

    location = params.get("location", "US")
    body = {
        "name": bucket,
        "location": location,
    }

    url = f"{STORAGE_API}/b?project={project}"
    data, status = make_request("POST", url, headers, body)

    if status not in (200, 201):
        return {"success": False, "error": data.get("error", f"HTTP {status}") if isinstance(data, dict) else f"HTTP {status}"}

    return {"success": True, "output": f"Bucket '{bucket}' created in project '{project}' (location: {location})"}


def list_objects(params, headers):
    bucket = params.get("bucket")
    valid, err = validate_bucket_name(bucket)
    if not valid:
        return {"success": False, "error": err}

    prefix = params.get("prefix", "")
    max_results = normalize_list_results(params)
    url = f"{STORAGE_API}/b/{bucket}/o?maxResults={max_results}"
    if prefix:
        import urllib.parse
        url += f"&prefix={urllib.parse.quote(prefix, safe='')}"

    data, status = make_request("GET", url, headers)

    if status != 200:
        return {"success": False, "error": data.get("error", f"HTTP {status}") if isinstance(data, dict) else f"HTTP {status}"}

    objects = []
    for obj in data.get("items", []):
        objects.append({
            "name": obj.get("name"),
            "size": obj.get("size"),
            "content_type": obj.get("contentType"),
            "updated": obj.get("updated"),
        })
    return {"success": True, "output": {"objects": objects, "count": len(objects), "bucket": bucket}}


def upload_object(params, headers):
    bucket = params.get("bucket")
    object_path = params.get("object_path")

    valid, err = validate_bucket_name(bucket)
    if not valid:
        return {"success": False, "error": err}
    valid, err = validate_object_path(object_path)
    if not valid:
        return {"success": False, "error": err}

    # Get content from inline text or local file
    content = params.get("content")
    local_path = params.get("local_path")

    if content is not None:
        data_bytes = content.encode("utf-8")
        content_type = "text/plain"
    elif local_path:
        # Feature gate: local file uploads disabled by default
        allowed, err = check_feature_allowed("allow_local_file_upload", "local file upload")
        if not allowed:
            return {"success": False, "error": err}

        # Validate local path is within allowed directories
        safe_path, err = resolve_safe_local_path(local_path)
        if err:
            return {"success": False, "error": err}

        if not os.path.exists(safe_path):
            return {"success": False, "error": f"Local file not found: {local_path}"}

        # Check file size before reading
        file_size = os.path.getsize(safe_path)
        if file_size > MAX_UPLOAD_BYTES:
            return {"success": False, "error": (
                f"File size ({file_size:,} bytes) exceeds maximum upload size "
                f"({MAX_UPLOAD_BYTES:,} bytes / {MAX_UPLOAD_BYTES // 1024 // 1024} MB). "
                "Use gsutil for larger files."
            )}

        with open(safe_path, "rb") as f:
            data_bytes = f.read()
        # Guess content type
        import mimetypes
        content_type = mimetypes.guess_type(safe_path)[0] or "application/octet-stream"
    else:
        return {"success": False, "error": "Either 'content' or 'local_path' is required for upload"}

    # Check size of inline content too
    if len(data_bytes) > MAX_UPLOAD_BYTES:
        return {"success": False, "error": (
            f"Content size ({len(data_bytes):,} bytes) exceeds maximum upload size "
            f"({MAX_UPLOAD_BYTES:,} bytes / {MAX_UPLOAD_BYTES // 1024 // 1024} MB)."
        )}

    import urllib.parse
    encoded_name = urllib.parse.quote(object_path, safe="")
    url = f"{UPLOAD_API}/b/{bucket}/o?uploadType=media&name={encoded_name}"

    upload_headers = dict(headers)
    upload_headers["Content-Type"] = content_type

    resp_data, status = make_request("POST", url, upload_headers, raw_body=data_bytes)

    if status not in (200, 201):
        return {"success": False, "error": resp_data.get("error", f"HTTP {status}") if isinstance(resp_data, dict) else f"HTTP {status}"}

    size = resp_data.get("size", len(data_bytes)) if isinstance(resp_data, dict) else len(data_bytes)
    return {"success": True, "output": f"Uploaded '{object_path}' to gs://{bucket}/{object_path} ({size} bytes)"}


def download_object(params, headers):
    bucket = params.get("bucket")
    object_path = params.get("object_path")

    valid, err = validate_bucket_name(bucket)
    if not valid:
        return {"success": False, "error": err}
    valid, err = validate_object_path(object_path)
    if not valid:
        return {"success": False, "error": err}

    # Check object size before downloading (metadata request)
    import urllib.parse
    encoded_name = urllib.parse.quote(object_path, safe="")

    meta_url = f"{STORAGE_API}/b/{bucket}/o/{encoded_name}"
    meta_data, meta_status = make_request("GET", meta_url, headers)
    if meta_status == 200 and isinstance(meta_data, dict):
        obj_size = int(meta_data.get("size", 0))
        if obj_size > MAX_DOWNLOAD_BYTES:
            return {"success": False, "error": (
                f"Object size ({obj_size:,} bytes) exceeds maximum download size "
                f"({MAX_DOWNLOAD_BYTES:,} bytes / {MAX_DOWNLOAD_BYTES // 1024 // 1024} MB). "
                "Use gsutil for larger files."
            )}

    url = f"{STORAGE_API}/b/{bucket}/o/{encoded_name}?alt=media"
    data, status = make_request("GET", url, headers)

    if status != 200:
        return {"success": False, "error": data.get("error", f"HTTP {status}") if isinstance(data, dict) else f"HTTP {status}"}

    local_path = params.get("local_path")
    if local_path:
        # Feature gate: local file downloads disabled by default
        allowed, err = check_feature_allowed("allow_local_file_download", "local file download")
        if not allowed:
            return {"success": False, "error": err}

        # Validate local path is within allowed directories
        safe_path, err = resolve_safe_local_path(local_path)
        if err:
            return {"success": False, "error": err}

        # Save to local file
        write_data = data if isinstance(data, bytes) else json.dumps(data).encode()
        os.makedirs(os.path.dirname(safe_path) or ".", exist_ok=True)
        with open(safe_path, "wb") as f:
            f.write(write_data)
        return {"success": True, "output": f"Downloaded gs://{bucket}/{object_path} to {local_path} ({len(write_data)} bytes)"}
    else:
        # Return content as text (truncated if too large)
        if isinstance(data, bytes):
            try:
                text = data.decode("utf-8")
            except UnicodeDecodeError:
                return {"success": True, "output": {"message": "Binary file — use local_path to download", "size": len(data)}}
        else:
            text = json.dumps(data)

        if len(text) > 10000:
            text = text[:10000] + f"\n... (truncated, {len(text)} total chars)"
        return {"success": True, "output": {"content": text, "object": object_path, "bucket": bucket}}


def delete_object(params, headers):
    bucket = params.get("bucket")
    object_path = params.get("object_path")

    valid, err = validate_bucket_name(bucket)
    if not valid:
        return {"success": False, "error": err}
    valid, err = validate_object_path(object_path)
    if not valid:
        return {"success": False, "error": err}

    import urllib.parse
    encoded_name = urllib.parse.quote(object_path, safe="")
    url = f"{STORAGE_API}/b/{bucket}/o/{encoded_name}"

    data, status = make_request("DELETE", url, headers)

    if status not in (200, 204):
        return {"success": False, "error": data.get("error", f"HTTP {status}") if isinstance(data, dict) else f"HTTP {status}"}

    return {"success": True, "output": f"Deleted gs://{bucket}/{object_path}"}


def get_object_metadata(params, headers):
    bucket = params.get("bucket")
    object_path = params.get("object_path")

    valid, err = validate_bucket_name(bucket)
    if not valid:
        return {"success": False, "error": err}
    valid, err = validate_object_path(object_path)
    if not valid:
        return {"success": False, "error": err}

    import urllib.parse
    encoded_name = urllib.parse.quote(object_path, safe="")
    url = f"{STORAGE_API}/b/{bucket}/o/{encoded_name}"

    data, status = make_request("GET", url, headers)

    if status != 200:
        return {"success": False, "error": data.get("error", f"HTTP {status}") if isinstance(data, dict) else f"HTTP {status}"}

    return {
        "success": True,
        "output": {
            "name": data.get("name"),
            "bucket": bucket,
            "size": data.get("size"),
            "content_type": data.get("contentType"),
            "created": data.get("timeCreated"),
            "updated": data.get("updated"),
            "md5": data.get("md5Hash"),
            "storage_class": data.get("storageClass"),
        }
    }


# ── Main entry point ──────────────────────────────────────────────

ACTIONS = {
    "list_buckets": list_buckets,
    "create_bucket": create_bucket,
    "list_objects": list_objects,
    "upload_object": upload_object,
    "download_object": download_object,
    "delete_object": delete_object,
    "get_object_metadata": get_object_metadata,
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
        "create_bucket": "allow_create_bucket",
        "delete_object": "allow_delete_objects",
    }
    if action in feature_gates:
        allowed, err = check_feature_allowed(feature_gates[action], action)
        if not allowed:
            print(json.dumps({"success": False, "error": err}))
            sys.exit(0)

    # ── Config enforcement: allowed buckets ──────────────────────
    bucket_actions = ["list_objects", "upload_object", "download_object", "delete_object", "get_object_metadata"]
    if action in bucket_actions:
        bucket = params.get("bucket", "")
        if bucket:
            # Validate bucket name format first
            valid, err = validate_bucket_name(bucket)
            if not valid:
                print(json.dumps({"success": False, "error": err}))
                sys.exit(0)
            allowed, err = check_resource_allowed("buckets", bucket)
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
