#!/usr/bin/env python3
"""Run ComfyUI img2img workflows: upload an image, inject prompt/params, download result."""

import argparse
import json
import os
import sys
import time
from pathlib import Path
from urllib.parse import urljoin

import requests


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Run a ComfyUI img2img workflow"
    )
    parser.add_argument("--workflow", required=True, help="Path to workflow JSON file")
    parser.add_argument("--image", required=True, help="Path to input image")
    parser.add_argument("--prompt", default="", help="Text prompt")
    parser.add_argument("--width", type=int, default=None, help="Output width")
    parser.add_argument("--height", type=int, default=None, help="Output height")
    parser.add_argument("--steps", type=int, default=None, help="Sampling steps")
    parser.add_argument("--denoise", type=float, default=None, help="Denoising strength")
    parser.add_argument("--output", default="data/output.png", help="Output image path")
    parser.add_argument("--url", default="http://localhost:8188", help="ComfyUI server URL")
    parser.add_argument("--timeout", type=int, default=300, help="Max wait seconds")
    args = parser.parse_args()

    base_url = args.url.rstrip("/")

    # 1. Read workflow JSON
    with open(args.workflow, "r") as f:
        workflow = json.load(f)

    # 2. Extract _personal_agent metadata
    meta = workflow.pop("_personal_agent", {})

    input_image_node = meta.get("input_image_node")
    prompt_node = meta.get("prompt_node")
    prompt_field = meta.get("prompt_field", "text")
    extra_params = meta.get("extra_params", {})

    if not input_image_node:
        print("Error: workflow has no input_image_node in _personal_agent", file=sys.stderr)
        sys.exit(1)

    # 3. Upload image to ComfyUI
    image_path = Path(args.image)
    if not image_path.is_file():
        print(f"Error: image not found: {args.image}", file=sys.stderr)
        sys.exit(1)

    upload_url = f"{base_url}/upload/image"
    with open(image_path, "rb") as img_file:
        try:
            resp = requests.post(
                upload_url,
                files={"image": (image_path.name, img_file)},
                data={"overwrite": "true"},
                timeout=30,
            )
            resp.raise_for_status()
        except requests.RequestException as e:
            print(f"Error uploading image to ComfyUI: {e}", file=sys.stderr)
            sys.exit(1)

    # 4. Set the uploaded filename in the LoadImage node
    uploaded_name = resp.json().get("name", image_path.name)
    workflow[input_image_node]["inputs"]["image"] = uploaded_name

    # 5. Inject prompt
    if prompt_node and prompt_node != "_none_" and prompt_node in workflow:
        workflow[prompt_node]["inputs"][prompt_field] = args.prompt

    # 6. Inject extra_params (width, height, steps)
    if args.width is not None:
        wnode = extra_params.get("width_node")
        if wnode and wnode in workflow:
            workflow[wnode]["inputs"]["width"] = args.width
    if args.height is not None:
        hnode = extra_params.get("height_node")
        if hnode and hnode in workflow:
            workflow[hnode]["inputs"]["height"] = args.height
    if args.steps is not None:
        snode = extra_params.get("steps_node")
        if snode and snode in workflow:
            workflow[snode]["inputs"]["steps"] = args.steps

    # 7. Inject denoise (find KSampler node and set denoise)
    if args.denoise is not None:
        for nid, node in workflow.items():
            if isinstance(node, dict) and node.get("class_type") == "KSampler":
                node["inputs"]["denoise"] = args.denoise
                break

    # 8. POST /prompt
    prompt_url = f"{base_url}/prompt"
    try:
        resp = requests.post(prompt_url, json={"prompt": workflow}, timeout=30)
        resp.raise_for_status()
    except requests.RequestException as e:
        print(f"Error queuing ComfyUI prompt: {e}", file=sys.stderr)
        sys.exit(1)

    prompt_id = resp.json().get("prompt_id")
    if not prompt_id:
        print("Error: no prompt_id in ComfyUI response", file=sys.stderr)
        sys.exit(1)

    print(f"Queued prompt_id={prompt_id}", file=sys.stderr)

    # 9. Poll /history/{prompt_id}
    history_url = f"{base_url}/history/{prompt_id}"
    deadline = time.time() + args.timeout
    output_info = None

    while time.time() < deadline:
        time.sleep(2)
        try:
            resp = requests.get(history_url, timeout=10)
            resp.raise_for_status()
            data = resp.json()
        except requests.RequestException:
            continue

        entry = data.get(prompt_id)
        if entry is None:
            continue  # still queued

        status = entry.get("status", {})
        if status.get("completed") is False:
            continue  # still running

        if status.get("status_str") == "error":
            msgs = status.get("messages", [])
            err = next((m[1] for m in msgs if m[0] == "execution_error"), None)
            msg = "unknown error"
            if isinstance(err, dict):
                msg = f"{err.get('node_type', '?')}: {err.get('exception_message', '?')}"
            print(f"ComfyUI execution error: {msg}", file=sys.stderr)
            sys.exit(1)

        # Find output image
        outputs = entry.get("outputs", {})
        for node_out in outputs.values():
            images = node_out.get("images", [])
            if images:
                img = images[0]
                output_info = {
                    "filename": img.get("filename", ""),
                    "subfolder": img.get("subfolder", ""),
                    "type": img.get("type", "output"),
                }
                break
        break

    if output_info is None:
        print("Error: ComfyUI generation timed out or no image produced", file=sys.stderr)
        sys.exit(1)

    # 10. Download the image
    view_url = (
        f"{base_url}/view"
        f"?filename={output_info['filename']}"
        f"&subfolder={output_info['subfolder']}"
        f"&type={output_info['type']}"
    )
    try:
        resp = requests.get(view_url, timeout=30)
        resp.raise_for_status()
    except requests.RequestException as e:
        print(f"Error downloading image: {e}", file=sys.stderr)
        sys.exit(1)

    # 11. Save to output
    out_path = Path(args.output)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_bytes(resp.content)

    # 12. Print output path on stdout
    print(str(out_path.resolve()))


if __name__ == "__main__":
    main()
