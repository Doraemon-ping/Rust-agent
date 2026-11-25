import sys, zipfile, re, os, io
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8", errors="replace")
from pptx import Presentation

p = r"C:\Users\26257\Desktop\Rust-agent-技术架构.pptx"
print("exists", os.path.exists(p), "size", os.path.getsize(p), "mtime", __import__("time").strftime("%Y-%m-%d %H:%M:%S", __import__("time").localtime(os.path.getmtime(p))))

d = Presentation(p)
print("slides", len(d.slides), "w", d.slide_width, "h", d.slide_height)

fonts = {}
runs = 0
alltext = []
for i, s in enumerate(d.slides, 1):
    texts = []
    for sh in s.shapes:
        if sh.has_text_frame:
            for para in sh.text_frame.paragraphs:
                line = "".join(r.text for r in para.runs)
                if line.strip():
                    texts.append(line)
            for para in sh.text_frame.paragraphs:
                for r in para.runs:
                    runs += 1
                    fonts[r.font.name] = fonts.get(r.font.name, 0) + 1
    alltext.append((i, texts))

print("runs", runs)
print("fonts", fonts)
for i, t in alltext:
    print("=" * 20, "slide", i)
    for line in t:
        print(line)
