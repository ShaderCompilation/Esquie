#!/usr/bin/env python3
"""Generate the AppImage's license documentation from locked packaging inputs."""

import argparse
import hashlib
import html
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
INPUTS = Path(__file__).resolve().parent
CARGO_ABOUT_VERSION = "0.9.3"


def legal_files(directory):
    """Include upstream license files and NOTICEs, including mixed-license files."""
    return sorted(
        p for p in directory.iterdir()
        if p.is_file() and p.name.lower().startswith(
            ("license", "licence", "copying", "copyright", "notice")
        )
    )


def generate(output, offline):
    tool = os.environ.get("CARGO_ABOUT", "cargo-about")
    version = subprocess.check_output([tool, "--version"], text=True).strip()
    if version != f"cargo-about {CARGO_ABOUT_VERSION}":
        raise RuntimeError(f"Expected cargo-about {CARGO_ABOUT_VERSION}, got {version}")
    sources = json.loads((INPUTS / "sources.json").read_text())
    for name, source in sources.items():
        if hashlib.sha256((INPUTS / name).read_bytes()).hexdigest() != source["sha256"]:
            raise RuntimeError(f"License input checksum mismatch: {name}")

    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="esquie-licenses-", dir=output.parent) as tmp:
        stage = Path(tmp)
        command = [tool, "generate", "--locked", "--fail"]
        if offline:
            command.append("--offline")
        report_path = stage / "report.json"
        subprocess.run(command + ["--format", "json", "-o", str(report_path)], cwd=ROOT, check=True)
        report = json.loads(report_path.read_text())
        packages = report["crates"]
        supplements = {
            (s["crate"], s["version"]): INPUTS / name
            for name, s in sources.items() if "crate" in s
        }
        # A generic MIT text alone omits upstream copyright notices. Require
        # packaged originals or a reviewed, version-specific supplemental file.
        for license_entry in report["licenses"]:
            if not license_entry["text"].strip():
                raise RuntimeError("Empty dependency license text")
            if license_entry["id"] == "MIT" and license_entry["source_path"] is None:
                for used in license_entry["used_by"]:
                    crate = used["crate"]
                    key = (crate["name"], crate["version"])
                    if not legal_files(Path(crate["manifest_path"]).parent) and key not in supplements:
                        raise RuntimeError(f"Missing upstream copyright notice: {key}")
        subprocess.run(
            command + [str(INPUTS / "dependencies.hbs"), "-o", str(stage / "dependencies.html")],
            cwd=ROOT, check=True,
        )

        sections = ["<!doctype html><html lang=\"en\"><meta charset=\"utf-8\">",
                    "<title>Additional Cargo notices</title><h1>Additional Cargo notices</h1>"]
        inventory = []
        for package in packages:
            crate = package["package"]
            key = (crate["name"], crate["version"])
            files = legal_files(Path(crate["manifest_path"]).parent)
            if key in supplements:
                files.append(supplements[key])
            for file in files:
                title = f"{crate['name']} {crate['version']} — {file.name}"
                sections.append(f"<h2>{html.escape(title)}</h2><pre>{html.escape(file.read_text())}</pre>")
            inventory.append({k: crate.get(k) for k in ("name", "version", "license", "repository", "source")})
        sections.append("</html>")
        (stage / "cargo-notices.html").write_text("\n".join(sections) + "\n")
        (stage / "dependencies.json").write_text(json.dumps(inventory, indent=2) + "\n")
        for name in ("LICENSE", "THIRD_PARTY_NOTICES.md"):
            shutil.copyfile(ROOT / name, stage / name)
        for name in sources:
            if "crate" in sources[name]:
                continue  # Incorporated into cargo-notices.html for the matching version.
            shutil.copyfile(INPUTS / name, stage / name)
        shutil.copyfile(INPUTS / "sources.json", stage / "license-sources.json")
        shutil.copyfile(ROOT / "Cargo.lock", stage / "Cargo.lock")
        report_path.unlink()  # Raw cargo metadata contains machine-specific paths.
        output.mkdir(parents=True, exist_ok=True)
        for file in stage.iterdir():
            shutil.copyfile(file, output / file.name)
    print(f"Generated licenses for {len(packages)} crates in {output}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, default=ROOT / "target/licenses")
    parser.add_argument("--offline", action="store_true", help="Use only cached Cargo sources")
    args = parser.parse_args()
    generate(args.out.resolve(), args.offline)
