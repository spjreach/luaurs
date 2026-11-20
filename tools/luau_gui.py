"""Small Tkinter front-end for the Rust Luau CLI.

Run with:  python tools/luau_gui.py

Type code in the top pane, press Ctrl+Enter (or the Run button) to execute it
with the release luau.exe and see stdout/stderr in the bottom pane. Native
code generation (the JIT) is on by default; the toolbar toggles it and picks the
optimization (-O) and debug (-g) levels. A run is killed after TIMEOUT_SECONDS
so a runaway loop cannot freeze the machine.
"""

import os
import subprocess
import tempfile
import time
import tkinter as tk
from tkinter import filedialog, messagebox, scrolledtext
from tkinter import ttk

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_EXE = os.path.join(ROOT, "target", "release", "luau.exe")
TIMEOUT_SECONDS = 5000


class LuauGui:
    def __init__(self, root: tk.Tk) -> None:
        root.title("Luau (Rust port) scratchpad")
        root.geometry("900x700")

        self.exe = tk.StringVar(value=DEFAULT_EXE)
        self.status = tk.StringVar(value="Ready")
        self.codegen = tk.BooleanVar(value=True)
        self.opt_level = tk.StringVar(value="0")
        self.debug_level = tk.StringVar(value="2")

        toolbar = ttk.Frame(root, padding=(6, 6, 6, 0))
        toolbar.pack(fill=tk.X)
        ttk.Button(toolbar, text="Run (Ctrl+Enter)", command=self.run).pack(side=tk.LEFT)
        ttk.Button(toolbar, text="Clear output", command=self.clear_output).pack(
            side=tk.LEFT, padx=(6, 0)
        )
        ttk.Button(toolbar, text="Open...", command=self.open_file).pack(
            side=tk.LEFT, padx=(6, 0)
        )
        ttk.Button(toolbar, text="Save...", command=self.save_file).pack(
            side=tk.LEFT, padx=(6, 0)
        )
        ttk.Checkbutton(toolbar, text="Native codegen (JIT)", variable=self.codegen).pack(
            side=tk.LEFT, padx=(12, 0)
        )
        ttk.Label(toolbar, text="-O").pack(side=tk.LEFT, padx=(12, 0))
        ttk.Combobox(
            toolbar, textvariable=self.opt_level, values=("0", "1", "2"), width=3, state="readonly"
        ).pack(side=tk.LEFT)
        ttk.Label(toolbar, text="-g").pack(side=tk.LEFT, padx=(8, 0))
        ttk.Combobox(
            toolbar, textvariable=self.debug_level, values=("0", "1", "2"), width=3, state="readonly"
        ).pack(side=tk.LEFT)

        exe_row = ttk.Frame(root, padding=(6, 6, 6, 0))
        exe_row.pack(fill=tk.X)
        ttk.Label(exe_row, text="luau.exe:").pack(side=tk.LEFT)
        ttk.Entry(exe_row, textvariable=self.exe).pack(
            side=tk.LEFT, fill=tk.X, expand=True, padx=(6, 0)
        )

        panes = ttk.PanedWindow(root, orient=tk.VERTICAL)
        panes.pack(fill=tk.BOTH, expand=True, padx=6, pady=6)

        code_frame = ttk.Frame(panes)
        ttk.Label(code_frame, text="Code").pack(anchor=tk.W)
        self.code = scrolledtext.ScrolledText(
            code_frame, wrap=tk.NONE, undo=True, font=("Consolas", 11)
        )
        self.code.pack(fill=tk.BOTH, expand=True)
        self.code.insert(
            "1.0",
            'print("hello from Luau")\n'
            "local t = {}\n"
            "for i = 1, 5 do t[i] = i * i end\n"
            "print(table.concat(t, \", \"))\n",
        )
        self.code.bind("<Control-Return>", lambda _event: self.run())
        panes.add(code_frame, weight=3)

        out_frame = ttk.Frame(panes)
        ttk.Label(out_frame, text="Output").pack(anchor=tk.W)
        self.output = scrolledtext.ScrolledText(
            out_frame, wrap=tk.WORD, state=tk.DISABLED, font=("Consolas", 10)
        )
        self.output.pack(fill=tk.BOTH, expand=True)
        self.output.tag_configure("stderr", foreground="#b00020")
        panes.add(out_frame, weight=2)

        ttk.Label(root, textvariable=self.status, anchor=tk.W).pack(
            fill=tk.X, padx=6, pady=(0, 6)
        )

    def append(self, text: str, tag: str | None = None) -> None:
        self.output.configure(state=tk.NORMAL)
        self.output.insert(tk.END, text, tag)
        self.output.see(tk.END)
        self.output.configure(state=tk.DISABLED)

    def clear_output(self) -> None:
        self.output.configure(state=tk.NORMAL)
        self.output.delete("1.0", tk.END)
        self.output.configure(state=tk.DISABLED)

    def open_file(self) -> None:
        path = filedialog.askopenfilename(
            filetypes=[("Luau scripts", "*.luau"), ("All files", "*.*")]
        )
        if path:
            with open(path, "r", encoding="utf-8") as handle:
                self.code.delete("1.0", tk.END)
                self.code.insert("1.0", handle.read())
            self.status.set(f"Loaded {path}")

    def save_file(self) -> None:
        path = filedialog.asksaveasfilename(
            defaultextension=".luau",
            filetypes=[("Luau scripts", "*.luau"), ("All files", "*.*")],
        )
        if path:
            with open(path, "w", encoding="utf-8", newline="\n") as handle:
                handle.write(self.code.get("1.0", "end-1c"))
            self.status.set(f"Saved {path}")

    def run(self) -> None:
        exe = self.exe.get().strip()
        if not os.path.isfile(exe):
            messagebox.showerror("Luau", f"Executable not found:\n{exe}")
            return

        source = self.code.get("1.0", "end-1c")
        self.clear_output()
        self.status.set("Running...")

        fd, path = tempfile.mkstemp(prefix="luau_gui_", suffix=".luau")
        try:
            with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as handle:
                handle.write(source)
            args = [
                exe,
                "--codegen" if self.codegen.get() else "--no-codegen",
                f"-O{self.opt_level.get()}",
                f"-g{self.debug_level.get()}",
                path,
            ]
            started = time.perf_counter()
            completed = subprocess.run(
                args,
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                timeout=TIMEOUT_SECONDS,
            )
            elapsed = time.perf_counter() - started
        except subprocess.TimeoutExpired as expired:
            out = expired.stdout or ""
            err = expired.stderr or ""
            if isinstance(out, bytes):
                out = out.decode("utf-8", "replace")
            if isinstance(err, bytes):
                err = err.decode("utf-8", "replace")
            if out:
                self.append(out)
            if err:
                self.append(err, "stderr")
            self.append(f"\n[timeout after {TIMEOUT_SECONDS}s; process killed]\n", "stderr")
            self.status.set(f"Timed out after {TIMEOUT_SECONDS}s")
            return
        except OSError as error:
            self.append(f"{error}\n", "stderr")
            self.status.set("Failed to start luau.exe")
            return
        finally:
            try:
                os.remove(path)
            except OSError:
                pass

        if completed.stdout:
            self.append(completed.stdout)
        if completed.stderr:
            self.append(completed.stderr, "stderr")
        code = completed.returncode
        mode = "JIT" if self.codegen.get() else "interpreter"
        result = "Finished" if code == 0 else f"Exited with code {code}"
        self.status.set(f"{result} in {elapsed * 1000:.0f} ms ({mode}, {' '.join(args[1:-1])})")


def main() -> None:
    root = tk.Tk()
    LuauGui(root)
    root.mainloop()


if __name__ == "__main__":
    main()
