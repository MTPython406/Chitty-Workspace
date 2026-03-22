#!/usr/bin/env python3
"""Slack Socket Mode connection script for Chitty Workspace.

This script maintains a persistent WebSocket connection to Slack via Socket Mode.
It receives events (@mentions, DMs, slash commands) and communicates with the
Chitty Workspace platform via stdin/stdout NDJSON protocol.

Protocol (stdout → platform):
  {"type":"ready","message":"Connected to Slack workspace XYZ"}
  {"type":"heartbeat"}
  {"type":"event","event_id":"mention","correlation_id":"uuid","data":{...}}
  {"type":"log","level":"info","message":"..."}
  {"type":"error","message":"...","fatal":false}

Protocol (stdin ← platform):
  {"type":"response","correlation_id":"uuid","data":{"text":"agent response","channel":"C123","thread_ts":"123.456"}}
  {"type":"shutdown"}
"""

import asyncio
import json
import sys
import os
import uuid
import signal
from datetime import datetime

# Ensure stdout is line-buffered for NDJSON
sys.stdout.reconfigure(line_buffering=True)

# ---------------------------------------------------------------------------
# NDJSON helpers
# ---------------------------------------------------------------------------

def send_message(msg: dict):
    """Send a JSON message to the platform via stdout."""
    print(json.dumps(msg, ensure_ascii=False), flush=True)

def send_ready(message: str = ""):
    send_message({"type": "ready", "message": message})

def send_heartbeat():
    send_message({"type": "heartbeat"})

def send_event(event_id: str, data: dict, correlation_id: str = None):
    send_message({
        "type": "event",
        "event_id": event_id,
        "correlation_id": correlation_id or str(uuid.uuid4()),
        "data": data,
    })

def send_log(message: str, level: str = "info"):
    send_message({"type": "log", "level": level, "message": message})

def send_error(message: str, fatal: bool = False):
    send_message({"type": "error", "message": message, "fatal": fatal})

# ---------------------------------------------------------------------------
# Credential loading
# ---------------------------------------------------------------------------

def get_bot_token() -> str:
    """Get Slack bot token from environment (set by platform from keyring)."""
    token = os.environ.get("CHITTY_CRED_OAUTH_SLACK_ACCESS_TOKEN")
    if not token:
        # Fallback: try direct env var or keyring
        token = os.environ.get("SLACK_BOT_TOKEN")
    if not token:
        try:
            import keyring
            token = keyring.get_password("chitty-workspace", "oauth_slack_access_token")
        except Exception:
            pass
    return token

def get_app_token() -> str:
    """Get Slack App-Level Token from environment (set by platform from keyring)."""
    token = os.environ.get("CHITTY_CRED_SLACK_APP_TOKEN")
    if not token:
        token = os.environ.get("SLACK_APP_TOKEN")
    if not token:
        try:
            import keyring
            token = keyring.get_password("chitty-workspace", "slack_app_token")
        except Exception:
            pass
    return token

# ---------------------------------------------------------------------------
# Stdin reader — receives responses from platform
# ---------------------------------------------------------------------------

class ResponseRouter:
    """Routes platform responses back to the correct Slack channel/thread."""

    def __init__(self, web_client):
        self.web_client = web_client
        self.pending = {}  # correlation_id → event metadata

    def register_event(self, correlation_id: str, channel: str, thread_ts: str = None, user: str = None):
        self.pending[correlation_id] = {
            "channel": channel,
            "thread_ts": thread_ts,
            "user": user,
        }

    async def handle_response(self, msg: dict):
        """Handle a response from the platform and post it to Slack."""
        correlation_id = msg.get("correlation_id", "")
        data = msg.get("data", {})
        text = data.get("text", "")

        if not text:
            send_log(f"Empty response for correlation {correlation_id}", "warn")
            return

        # Look up where to send the response
        event_meta = self.pending.pop(correlation_id, None)
        if not event_meta:
            # Fallback: use channel/thread from response data
            channel = data.get("channel")
            thread_ts = data.get("thread_ts")
        else:
            channel = data.get("channel") or event_meta.get("channel")
            thread_ts = data.get("thread_ts") or event_meta.get("thread_ts")

        if not channel:
            send_log(f"No channel for response {correlation_id}", "error")
            return

        try:
            kwargs = {"channel": channel, "text": text}
            if thread_ts:
                kwargs["thread_ts"] = thread_ts

            await self.web_client.chat_postMessage(**kwargs)
            send_log(f"Posted response to {channel}" + (f" (thread {thread_ts})" if thread_ts else ""))
        except Exception as e:
            send_error(f"Failed to post response to Slack: {e}")


async def stdin_reader(router: ResponseRouter, shutdown_event: asyncio.Event):
    """Read NDJSON messages from stdin (platform → script)."""
    loop = asyncio.get_event_loop()
    reader = asyncio.StreamReader()
    protocol = asyncio.StreamReaderProtocol(reader)
    await loop.connect_read_pipe(lambda: protocol, sys.stdin)

    while not shutdown_event.is_set():
        try:
            line = await asyncio.wait_for(reader.readline(), timeout=1.0)
            if not line:
                # stdin closed — platform is shutting down
                send_log("stdin closed, shutting down")
                shutdown_event.set()
                break

            line = line.decode("utf-8").strip()
            if not line:
                continue

            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                send_log(f"Invalid JSON from platform: {line}", "warn")
                continue

            msg_type = msg.get("type", "")
            if msg_type == "shutdown":
                send_log("Received shutdown command")
                shutdown_event.set()
                break
            elif msg_type == "response":
                await router.handle_response(msg)
            elif msg_type == "config_update":
                send_log("Config update received (not yet implemented)")
            else:
                send_log(f"Unknown message type from platform: {msg_type}", "warn")

        except asyncio.TimeoutError:
            continue
        except Exception as e:
            send_error(f"Error reading stdin: {e}")
            await asyncio.sleep(1)

# ---------------------------------------------------------------------------
# Socket Mode event handler
# ---------------------------------------------------------------------------

async def handle_socket_mode_event(client, req, router: ResponseRouter, bot_user_id: str):
    """Handle an incoming Socket Mode event."""
    from slack_sdk.socket_mode.response import SocketModeResponse
    from slack_sdk.socket_mode.request import SocketModeRequest

    # Acknowledge immediately (Slack requires response within 3 seconds)
    response = SocketModeResponse(envelope_id=req.envelope_id)
    await client.send_socket_mode_response(response)

    req_type = req.type

    if req_type == "events_api":
        event = req.payload.get("event", {})
        event_type = event.get("type", "")
        user = event.get("user", "")
        text = event.get("text", "")
        channel = event.get("channel", "")
        thread_ts = event.get("thread_ts") or event.get("ts", "")

        # Skip bot's own messages
        if user == bot_user_id:
            return
        # Skip message subtypes (joins, leaves, etc.)
        if event.get("subtype"):
            return

        # Clean up text: remove bot mention prefix
        if bot_user_id:
            text = text.replace(f"<@{bot_user_id}>", "").strip()

        if event_type == "app_mention":
            correlation_id = str(uuid.uuid4())
            router.register_event(correlation_id, channel, thread_ts, user)
            send_event("mention", {
                "user": user,
                "text": text,
                "channel": channel,
                "thread_ts": thread_ts,
                "timestamp": datetime.utcnow().isoformat() + "Z",
            }, correlation_id)

        elif event_type == "message" and event.get("channel_type") == "im":
            correlation_id = str(uuid.uuid4())
            router.register_event(correlation_id, channel, thread_ts, user)
            send_event("dm", {
                "user": user,
                "text": text,
                "channel": channel,
                "thread_ts": thread_ts,
                "timestamp": datetime.utcnow().isoformat() + "Z",
            }, correlation_id)

    elif req_type == "slash_commands":
        command = req.payload.get("command", "")
        text = req.payload.get("text", "")
        user = req.payload.get("user_id", "")
        channel = req.payload.get("channel_id", "")

        correlation_id = str(uuid.uuid4())
        router.register_event(correlation_id, channel, None, user)
        send_event("slash_command", {
            "user": user,
            "text": text,
            "command": command,
            "channel": channel,
            "timestamp": datetime.utcnow().isoformat() + "Z",
        }, correlation_id)

    elif req_type == "interactive":
        send_log(f"Interactive event received (not yet handled): {req.payload.get('type', 'unknown')}")

# ---------------------------------------------------------------------------
# Heartbeat loop
# ---------------------------------------------------------------------------

async def heartbeat_loop(shutdown_event: asyncio.Event, interval: int = 25):
    """Send periodic heartbeats to the platform."""
    while not shutdown_event.is_set():
        try:
            await asyncio.wait_for(shutdown_event.wait(), timeout=interval)
        except asyncio.TimeoutError:
            send_heartbeat()

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

async def main():
    # Validate credentials
    bot_token = get_bot_token()
    app_token = get_app_token()

    if not bot_token:
        send_error("Slack bot token not found. Complete the Slack package setup first.", fatal=True)
        return

    if not app_token:
        send_error("Slack App-Level Token not found. Add it in the Slack package setup (xapp-...).", fatal=True)
        return

    if not app_token.startswith("xapp-"):
        send_error(f"Invalid App-Level Token format. Must start with 'xapp-', got: {app_token[:10]}...", fatal=True)
        return

    try:
        from slack_sdk.web.async_client import AsyncWebClient
        from slack_sdk.socket_mode.aiohttp import SocketModeClient
    except ImportError:
        send_error("slack_sdk not installed. Run: pip install slack_sdk aiohttp", fatal=True)
        return

    # Create Slack clients
    web_client = AsyncWebClient(token=bot_token)
    socket_client = SocketModeClient(app_token=app_token, web_client=web_client)

    # Get bot user ID (to filter own messages)
    bot_user_id = ""
    try:
        auth_response = await web_client.auth_test()
        bot_user_id = auth_response.get("user_id", "")
        team_name = auth_response.get("team", "unknown")
        bot_name = auth_response.get("user", "unknown")
        send_log(f"Authenticated as {bot_name} in workspace {team_name}")
    except Exception as e:
        send_error(f"Failed to authenticate with Slack: {e}", fatal=True)
        return

    # Set up response router
    router = ResponseRouter(web_client)
    shutdown_event = asyncio.Event()

    # Handle OS signals
    for sig in (signal.SIGINT, signal.SIGTERM):
        try:
            asyncio.get_event_loop().add_signal_handler(sig, lambda: shutdown_event.set())
        except NotImplementedError:
            # Windows doesn't support add_signal_handler
            pass

    # Register Socket Mode event handler
    socket_client.socket_mode_request_listeners.append(
        lambda client, req: asyncio.ensure_future(
            handle_socket_mode_event(client, req, router, bot_user_id)
        )
    )

    # Start Socket Mode connection
    try:
        await socket_client.connect()
        send_ready(f"Connected to Slack workspace '{team_name}' as @{bot_name}")
    except Exception as e:
        send_error(f"Failed to connect Socket Mode: {e}", fatal=True)
        return

    # Run heartbeat + stdin reader concurrently
    try:
        await asyncio.gather(
            heartbeat_loop(shutdown_event, interval=25),
            stdin_reader(router, shutdown_event),
        )
    except Exception as e:
        send_error(f"Connection error: {e}")
    finally:
        send_log("Disconnecting from Slack...")
        try:
            await socket_client.disconnect()
        except Exception:
            pass
        send_log("Disconnected")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
