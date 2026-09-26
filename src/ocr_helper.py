"""Optional local OCR helper. Crops in memory and returns bounded TSV text only."""
import csv
import hashlib
import io
import json
import re
import subprocess
import sys
import threading

MAX_INPUT = 20 * 1024 * 1024
MAX_OUTPUT = 1024 * 1024
MAX_DIMENSION = 8192
MAX_LINES = 512
HANGUL_WORD = re.compile(r"^[\u1100-\u11ff\u3130-\u318f\uac00-\ud7af]+$")


def fail(reason):
    print(json.dumps({"error": reason}, separators=(",", ":")))
    raise SystemExit(0)


def join_line_words(words):
    text = ""
    previous = None
    for word in words:
        if previous is not None:
            gap = word["left"] - previous["right"]
            close_hangul = (
                HANGUL_WORD.fullmatch(previous["text"])
                and HANGUL_WORD.fullmatch(word["text"])
                and gap <= min(previous["height"], word["height"]) * 0.2
            )
            text += "" if close_hangul else " "
        text += word["text"]
        previous = word
    return text


class OcrOutputTooLarge(Exception):
    pass


def run_bounded_process(command, input_bytes, timeout):
    process = subprocess.Popen(
        command,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    output = bytearray()
    too_large = threading.Event()
    read_errors = []
    write_errors = []

    def kill_process():
        try:
            process.kill()
        except ProcessLookupError:
            pass

    def collect_output():
        try:
            while True:
                remaining = MAX_OUTPUT - len(output)
                read = getattr(process.stdout, "read1", process.stdout.read)
                chunk = read(min(65536, remaining + 1))
                if not chunk:
                    return
                if len(chunk) > remaining:
                    too_large.set()
                    kill_process()
                    return
                output.extend(chunk)
        except OSError as error:
            read_errors.append(error)
        finally:
            process.stdout.close()

    def send_input():
        try:
            process.stdin.write(input_bytes)
            process.stdin.flush()
        except BrokenPipeError:
            pass
        except OSError as error:
            write_errors.append(error)
        finally:
            try:
                process.stdin.close()
            except OSError:
                pass

    reader = threading.Thread(target=collect_output)
    writer = threading.Thread(target=send_input)
    reader.start()
    writer.start()
    timed_out = False
    try:
        process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        kill_process()
        process.wait()
    writer.join()
    reader.join()

    if too_large.is_set():
        raise OcrOutputTooLarge()
    if timed_out:
        raise subprocess.TimeoutExpired(command, timeout)
    if read_errors:
        raise read_errors[0]
    if write_errors:
        raise write_errors[0]
    if process.returncode:
        raise subprocess.CalledProcessError(process.returncode, command)
    return bytes(output)


if sys.argv[1:] == ["--self-test"]:
    adjacent = [
        {"text": "열", "left": 33, "right": 97, "height": 36},
        {"text": "기", "left": 73, "right": 102, "height": 55},
    ]
    separated = [
        {"text": "안녕", "left": 10, "right": 72, "height": 40},
        {"text": "세상", "left": 92, "right": 154, "height": 40},
    ]
    assert join_line_words(adjacent) == "열기"
    assert join_line_words(separated) == "안녕 세상"
    raise SystemExit(0)

if sys.argv[1:] == ["--output-limit-self-test"]:
    child = "import sys; sys.stdout.buffer.write(b'x' * {})".format(MAX_OUTPUT + 1)
    try:
        run_bounded_process([sys.executable, "-c", child], b"", 3.0)
    except OcrOutputTooLarge:
        raise SystemExit(0)
    raise AssertionError("subprocess output limit was not enforced")


try:
    from PIL import Image
except Exception:
    fail("ocr_unavailable")


def region_fingerprint(crop):
    """Hash a coarse RGB view to reduce sensitivity to capture noise.

    Small changes can still cross a quantization boundary, and changes below
    the normalized image resolution may not change the fingerprint.
    """
    longest = max(crop.size)
    width = max(1, round(crop.width * min(1.0, 128 / longest)))
    height = max(1, round(crop.height * min(1.0, 128 / longest)))
    resampling = getattr(getattr(Image, "Resampling", Image), "BOX")
    normalized = crop.convert("RGB").resize((width, height), resampling)
    prefix = bytes((width >> 8, width & 255, height >> 8, height & 255))
    quantized = bytes(channel // 16 for channel in normalized.tobytes())
    return hashlib.sha256(b"ocr-region-v1" + prefix + quantized).hexdigest()


def stable_crop(current, reference):
    """Treat only full-resolution per-channel +/-1 capture dithering as equal.

    No blur/downsampling or accumulated tolerance: every refresh is compared
    directly with the original caller Observation, not the preceding refresh.
    """
    from PIL import ImageChops
    if current.size != reference.size:
        return current
    diff = ImageChops.difference(current.convert("RGB"), reference.convert("RGB"))
    return reference if all(high <= 1 for low, high in diff.getextrema()) else current


if sys.argv[1:] == ["--fingerprint-self-test"]:
    from PIL import ImageDraw

    original = Image.new("RGB", (275, 100), (232, 232, 232))
    noisy = Image.merge(
        "RGB",
        tuple(channel.point(lambda value: min(255, value + 1)) for channel in original.split()),
    )
    changed = original.copy()
    ImageDraw.Draw(changed).rectangle((20, 10, 250, 55), fill=(64, 120, 200))
    boundary = Image.new("RGB", (275, 100), (15, 15, 15))
    boundary_change = Image.new("RGB", (275, 100), (16, 16, 16))
    assert region_fingerprint(original) == region_fingerprint(noisy)
    assert region_fingerprint(original) != region_fingerprint(changed)
    assert region_fingerprint(boundary) != region_fingerprint(boundary_change)
    assert stable_crop(boundary_change, boundary) is boundary
    assert stable_crop(changed, original) is changed
    two_levels = Image.new("RGB", original.size, (234, 232, 232))
    assert stable_crop(two_levels, original) is two_levels
    one_pixel = original.copy()
    one_pixel.putpixel((13, 17), (234, 232, 232))
    assert stable_crop(one_pixel, original) is one_pixel
    raise SystemExit(0)

try:
    split = int(sys.argv[7]) if len(sys.argv) == 8 else None
    bound = MAX_INPUT * (2 if split is not None else 1)
    raw = sys.stdin.buffer.read(bound + 1)
    if not raw or len(raw) > bound or (split is not None and not (0 < split <= MAX_INPUT and 0 < len(raw)-split <= MAX_INPUT)):
        fail("malformed_image")
    image = Image.open(io.BytesIO(raw[:split] if split is not None else raw))
    image.load()
except Exception:
    fail("malformed_image")

if image.format != "PNG" or image.width > MAX_DIMENSION or image.height > MAX_DIMENSION:
    fail("malformed_image")

try:
    x, y, width, height = (int(value) for value in sys.argv[1:5])
    languages = sys.argv[5]
    timeout = min(3.0, max(0.1, float(sys.argv[6])))
except Exception:
    fail("invalid_ocr_request")
if (x < 0 or y < 0 or width <= 0 or height <= 0
        or x + width > image.width or y + height > image.height
        or not re.fullmatch(r"(eng|kor)(\+(eng|kor))?", languages)):
    fail("invalid_ocr_request")

try:
    crop_image = image.crop((x, y, x + width, y + height))
    if split is not None:
        reference = Image.open(io.BytesIO(raw[split:]))
        if reference.format != "PNG" or reference.size != image.size:
            fail("malformed_image")
        reference.load()
        crop_image = stable_crop(crop_image, reference.crop((x, y, x + width, y + height)))
    scale = 2 if max(width, height) <= 4096 and width * height <= 1_000_000 else 1
    if scale > 1:
        resampling = getattr(getattr(Image, "Resampling", Image), "LANCZOS")
        tesseract_image = crop_image.resize((width * scale, height * scale), resampling)
    else:
        tesseract_image = crop_image
    crop = io.BytesIO()
    tesseract_image.save(crop, format="PNG")
    crop_bytes = crop.getvalue()
    region_hash = region_fingerprint(crop_image)
    tsv_output = run_bounded_process(
        ["tesseract", "stdin", "stdout", "-l", languages, "--psm", "6", "tsv"],
        crop_bytes,
        timeout,
    )
    rows = csv.DictReader(io.StringIO(tsv_output.decode("utf-8", "replace")), delimiter="\t")
    lines = {}
    for row in rows:
        if row.get("level") != "5":
            continue
        text = re.sub(r"\s+", " ", row.get("text", "")).strip()
        try:
            confidence = float(row.get("conf", "-1"))
            left = int(row["left"]) / scale
            top = int(row["top"]) / scale
            word_width = int(row["width"]) / scale
            word_height = int(row["height"]) / scale
        except Exception:
            continue
        if (not text or not confidence >= 0 or not 0 <= left < width
                or not 0 <= top < height or word_width <= 0 or word_height <= 0
                or left + word_width > width or top + word_height > height):
            continue
        key = tuple(row.get(field, "") for field in ("page_num", "block_num", "par_num", "line_num"))
        line = lines.setdefault(key, {
            "words": [], "left": left, "top": top,
            "right": left + word_width, "bottom": top + word_height,
            "confidence": [],
        })
        line["words"].append({
            "text": text,
            "left": left,
            "right": left + word_width,
            "height": word_height,
        })
        line["left"] = min(line["left"], left)
        line["top"] = min(line["top"], top)
        line["right"] = max(line["right"], left + word_width)
        line["bottom"] = max(line["bottom"], top + word_height)
        line["confidence"].append(confidence)
    elements = []
    truncated = len(lines) > MAX_LINES
    for index, line in enumerate(lines.values()):
        if index >= MAX_LINES:
            break
        elements.append({
            "id": "ocr-line-{}".format(index),
            "text": join_line_words(line["words"])[:120],
            "x": line["left"], "y": line["top"],
            "width": line["right"] - line["left"],
            "height": line["bottom"] - line["top"],
            "confidence": sum(line["confidence"]) / len(line["confidence"]),
        })
    print(json.dumps({
        "elements": elements,
        "truncated": truncated,
        "region_hash": region_hash,
    }, ensure_ascii=False, separators=(",", ":")))
except subprocess.TimeoutExpired:
    fail("ocr_timeout")
except OcrOutputTooLarge:
    fail("ocr_output_too_large")
except FileNotFoundError:
    fail("ocr_unavailable")
except subprocess.CalledProcessError:
    fail("ocr_unavailable")
except Exception:
    fail("ocr_failed")
