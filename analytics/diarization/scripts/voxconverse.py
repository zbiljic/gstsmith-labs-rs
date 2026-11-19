"""Labelled broadcast audio through `cactuswhistle ! diarizer`.

usage:
  voxconverse.py extract PARQUET DIR
      Write each recording of a VoxConverse parquet shard as DIR/ID.wav,
      with its speaker turns as DIR/ID.rttm.
  voxconverse.py evaluate DIR WHISTLE MODEL [--jobs N] [--only ID,...] [PROPERTY=VALUE ...]
      Transcribe each DIR/ID.wav with cactuswhistle, label the cues with
      diarizer (extra PROPERTY=VALUE pairs go to diarizer), and score the
      labels against DIR/ID.rttm. Needs gst-launch-1.0 and both plugins on
      GST_PLUGIN_PATH.

A cue's speaker is the one who talks most during it. Hypothesis speakers
are mapped one to one onto true speakers so that the most cues agree; a cue
is correct when its speaker maps to its true speaker, and a cue with an
unknown speaker is wrong. Speaker counts are compared with the true
speakers who own at least one cue, since a person who only says a word or
two cannot be found from cues. A found speaker left out of the mapping is
a split (one voice found twice, or a voice the labels leave out); a true
speaker left out is missed. Purity is the share of a cue's speech that its
own speaker says, counting overlapped speech for everyone talking: what
labelling whole cues can reach at most.
"""

import os
import re
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor

FIELD = re.compile(r"(\w+)=\((\w+)\)(\"(?:[^\"\\]|\\.)*\"|[^,;]*)")
# The longest recording takes a few minutes; this only stops a stalled run.
TIMEOUT_S = 3600


def extract(parquet, out):
    import pyarrow.parquet as pq

    os.makedirs(out, exist_ok=True)
    count = 0
    for batch in pq.ParquetFile(parquet).iter_batches(batch_size=1):
        row = batch.to_pylist()[0]
        count += 1
        name = os.path.splitext(os.path.basename(row["audio"]["path"]))[0]
        with open(os.path.join(out, name + ".wav"), "wb") as f:
            f.write(row["audio"]["bytes"])
        turns = sorted(
            zip(row["timestamps_start"], row["timestamps_end"], row["speakers"])
        )
        with open(os.path.join(out, name + ".rttm"), "w") as f:
            for start, end, speaker in turns:
                f.write(
                    f"SPEAKER {name} 1 {start:.3f} {end - start:.3f}"
                    f" <NA> <NA> {speaker} <NA> <NA>\n"
                )
    print(f"{count} recordings in {out}")


def run(wav, whistle, model, properties):
    """The diarizer's cues for one recording: (start s, end s, n or None)."""
    command = [
        "gst-launch-1.0", "-m",
        "filesrc", f"location={wav}", "!", "decodebin", "!",
        "audioconvert", "!", "audioresample", "!",
        "audio/x-raw,format=F32LE,rate=16000,channels=1", "!", "tee", "name=t",
        "t.", "!", "queue", "!",
        "cactuswhistle", f"model-file={whistle}", "language=en", "!", "d.text_sink",
        "t.", "!", "queue", "!", "d.audio_sink",
        "diarizer", "name=d", f"model-file={model}", *properties, "!", "fakesink",
    ]  # fmt: skip
    try:
        result = subprocess.run(
            command,
            capture_output=True,
            text=True,
            errors="replace",
            timeout=TIMEOUT_S,
        )
    except subprocess.TimeoutExpired:
        sys.exit(f"{wav}: gst-launch-1.0 ran for over {TIMEOUT_S} s")
    if result.returncode != 0 or "Got EOS" not in result.stdout:
        sys.exit(
            f"{wav}: gst-launch-1.0 failed\n{result.stdout[-2000:]}{result.stderr}"
        )
    cues = []
    for line in result.stdout.splitlines():
        if 'element "d"' not in line or "diarizer/speaker," not in line:
            continue
        body = line.split("diarizer/speaker,", 1)[1]
        fields = {key: value for key, _, value in FIELD.findall(body)}
        if "pts" not in fields or "duration" not in fields:
            sys.exit(f"{wav}: a cue without a timestamp or duration: {line}")
        start = int(fields["pts"]) / 1e9
        end = start + int(fields["duration"]) / 1e9
        cues.append((start, end, int(fields["n"]) if "n" in fields else None))
    return cues


def turns(rttm):
    out = []
    with open(rttm) as f:
        for line in f:
            parts = line.split()
            start = float(parts[3])
            out.append((start, start + float(parts[4]), parts[7]))
    return out


def score(cues, reference):
    import numpy as np
    from scipy.optimize import linear_sum_assignment

    rows = []
    for start, end, n in cues:
        spoken = {}
        for turn_start, turn_end, speaker in reference:
            overlap = min(end, turn_end) - max(start, turn_start)
            if overlap > 0:
                spoken[speaker] = spoken.get(speaker, 0.0) + overlap
        if spoken:
            owner = max(spoken, key=spoken.get)
            rows.append((owner, n, spoken[owner] / sum(spoken.values()), end - start))
    owners = sorted({owner for owner, _, _, _ in rows})
    found = sorted({n for _, _, n in cues if n is not None})
    agree = np.zeros((len(found), len(owners)))
    for owner, n, _, _ in rows:
        if n is not None:
            agree[found.index(n), owners.index(owner)] += 1
    mapped_found, mapped_owners = (
        linear_sum_assignment(-agree) if found and owners else ([], [])
    )
    # A pair that shares no cue is not a match.
    kept = [agree[f, o] > 0 for f, o in zip(mapped_found, mapped_owners)]
    mapped_found = [f for f, keep in zip(mapped_found, kept) if keep]
    mapped_owners = [o for o, keep in zip(mapped_owners, kept) if keep]
    unknown = [length for _, n, _, length in rows if n is None]
    return {
        "cues": len(cues),
        "speech": len(rows),
        "correct": int(agree[mapped_found, mapped_owners].sum()),
        "unknown": len(unknown),
        "longest_unknown": max(unknown, default=0.0),
        "purity": sum(purity for _, _, purity, _ in rows),
        "found": len(found),
        "owners": len(owners),
        "split": len(found) - len(mapped_found),
        "missed": len(owners) - len(mapped_owners),
        "speakers": len({speaker for _, _, speaker in reference}),
    }


def evaluate(args):
    directory, whistle, model, *rest = args
    jobs, only, properties = 3, None, []
    while rest:
        arg = rest.pop(0)
        if arg in ("--jobs", "--only") and not rest:
            sys.exit(f"{arg} needs a value")
        if arg == "--jobs":
            value = rest.pop(0)
            if not value.isdigit() or int(value) < 1:
                sys.exit(f"--jobs needs a positive number, not {value}")
            jobs = int(value)
        elif arg == "--only":
            only = set(rest.pop(0).split(","))
        else:
            properties.append(arg)
    available = {name[:-5] for name in os.listdir(directory) if name.endswith(".rttm")}
    if only is not None and only - available:
        sys.exit(f"not in {directory}: {', '.join(sorted(only - available))}")
    names = sorted(only if only is not None else available)
    if not names:
        sys.exit(f"no recordings in {directory}")

    def one(name):
        cues = run(os.path.join(directory, name + ".wav"), whistle, model, properties)
        return name, score(cues, turns(os.path.join(directory, name + ".rttm")))

    results = []
    print(
        f"{'recording':10} {'cues':>5} {'correct':>8} {'unknown':>8}"
        f" {'speakers':>9} {'true':>5} {'split':>6} {'missed':>7}"
    )
    with ThreadPoolExecutor(jobs) as pool:
        for name, r in pool.map(one, names):
            results.append(r)
            share = r["unknown"] / max(r["speech"], 1)
            print(
                f"{name:10} {r['speech']:5d} {100 * r['correct'] / max(r['speech'], 1):7.1f}%"
                f" {100 * share:7.1f}% {r['found']:9d} {r['owners']:5d}"
                f" {r['split']:6d} {r['missed']:7d}",
                flush=True,
            )
            if share > 0.5:
                print(
                    f"warning: most of {name}'s cues have no speaker; if their"
                    " audio was lost, raise audio-buffer-duration",
                    flush=True,
                )
    total = {key: sum(r[key] for r in results) for key in results[0]}
    longest = max(r["longest_unknown"] for r in results)
    exact = sum(1 for r in results if r["found"] == r["owners"])
    near = sum(1 for r in results if abs(r["found"] - r["owners"]) <= 1)
    print(
        f"\n{len(results)} recordings, {total['speech']} cues with speech"
        f" and {total['cues'] - total['speech']} outside the labelled turns."
        f"\nCues: {100 * total['correct'] / total['speech']:.1f}% correct,"
        f" {100 * total['unknown'] / total['speech']:.1f}% unknown"
        f" (the longest {longest:.2f} s), purity {100 * total['purity'] / total['speech']:.1f}%."
        f"\nSpeakers: {total['found']} found for {total['owners']}"
        f" ({total['speakers']} in the labels); {total['split']} split off,"
        f" {total['missed']} missed; exact in {exact}, within one in {near}."
    )


def main():
    if len(sys.argv) == 4 and sys.argv[1] == "extract":
        extract(sys.argv[2], sys.argv[3])
    elif len(sys.argv) >= 5 and sys.argv[1] == "evaluate":
        evaluate(sys.argv[2:])
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
