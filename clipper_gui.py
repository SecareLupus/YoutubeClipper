#!/usr/bin/env python3
"""Simple Tkinter GUI front-end for clipper.py."""

from __future__ import annotations

import shlex
import subprocess
import sys
import threading
from pathlib import Path
import importlib
from tkinter import filedialog, messagebox, scrolledtext, ttk
import tkinter as tk

try:
    from yt_dlp import YoutubeDL
except ModuleNotFoundError:  # pragma: no cover - optional dependency for title lookup
    YoutubeDL = None  # type: ignore[assignment]


class ClipperGUI(tk.Tk):
    def __init__(self) -> None:
        super().__init__()
        self.title("YouTube QuoteCutter")
        self.resizable(True, True)
        self.script_path = Path(__file__).with_name("clipper.py")
        self.process: subprocess.Popen[str] | None = None

        self._create_variables()
        self._build_layout()
        self._wire_variable_listeners()
        self._update_command_preview()
        self.after(250, self._maybe_offer_yt_dlp_update)

    def _create_variables(self) -> None:
        self.url_var = tk.StringVar()
        self.query_var = tk.StringVar()
        self.before_var = tk.StringVar(value="5")
        self.after_var = tk.StringVar(value="5")
        self.lang_var = tk.StringVar(value="en")
        self.output_var = tk.StringVar(value="clip.mp4")
        self.quality_var = tk.StringVar()
        self.verbose_var = tk.BooleanVar(value=False)
        self.command_preview_var = tk.StringVar()
        self.video_title_var = tk.StringVar(value="Enter a URL to preview title.")
        self.language_options = ["en", "es", "fr", "de", "pt", "it", "ja", "ko"]
        self.quality_options = {
            "Best available": "",
            "High (1080p)": "bestvideo[height<=1080]+bestaudio/best",
            "Medium (720p)": "bestvideo[height<=720]+bestaudio/best",
            "Audio only": "bestaudio/best",
        }
        self.lang_var.set(self.language_options[0])
        self.quality_var.set("Best available")
        self._title_fetch_after_id: str | None = None
        self._title_request_counter = 0
        self._offered_update = False

    def _build_layout(self) -> None:
        padding = {"padx": 8, "pady": 4, "sticky": "ew"}
        self.columnconfigure(1, weight=1)

        ttk.Label(self, text="YouTube URL").grid(row=0, column=0, **padding)
        ttk.Entry(self, textvariable=self.url_var).grid(row=0, column=1, **padding)
        ttk.Label(
            self,
            textvariable=self.video_title_var,
            foreground="#555555",
        ).grid(row=1, column=0, columnspan=2, padx=8, pady=(0, 4), sticky="w")

        ttk.Label(self, text="Query text").grid(row=2, column=0, **padding)
        ttk.Entry(self, textvariable=self.query_var).grid(row=2, column=1, **padding)

        ttk.Label(self, text="Include").grid(row=3, column=0, **padding)
        include_frame = ttk.Frame(self)
        include_frame.grid(row=3, column=1, **padding)
        ttk.Spinbox(
            include_frame, from_=0, to=60, increment=0.5, width=6, textvariable=self.before_var
        ).grid(row=0, column=0, padx=(0, 4))
        ttk.Label(include_frame, text="sec before").grid(row=0, column=1, padx=(4, 24))
        ttk.Spinbox(
            include_frame, from_=0, to=60, increment=0.5, width=6, textvariable=self.after_var
        ).grid(row=0, column=2, padx=(0, 4))
        ttk.Label(include_frame, text="sec after").grid(row=0, column=3, padx=(4, 0))

        ttk.Label(self, text="Language").grid(row=4, column=0, **padding)
        ttk.Combobox(
            self,
            textvariable=self.lang_var,
            values=self.language_options,
            state="normal",
        ).grid(row=4, column=1, **padding)

        ttk.Label(self, text="Output file").grid(row=5, column=0, **padding)
        output_frame = ttk.Frame(self)
        output_frame.grid(row=5, column=1, **padding)
        output_frame.columnconfigure(0, weight=1)
        ttk.Entry(output_frame, textvariable=self.output_var).grid(row=0, column=0, sticky="ew")
        ttk.Button(output_frame, text="Browse…", command=self._choose_output).grid(
            row=0, column=1, padx=(6, 0)
        )

        ttk.Label(self, text="Quality").grid(row=6, column=0, **padding)
        ttk.Combobox(
            self,
            textvariable=self.quality_var,
            values=list(self.quality_options.keys()),
            state="readonly",
        ).grid(row=6, column=1, **padding)

        ttk.Checkbutton(
            self,
            text="Nerd Details (verbose logs)",
            variable=self.verbose_var,
        ).grid(row=7, column=0, columnspan=2, padx=8, pady=(0, 4), sticky="w")

        ttk.Label(self, text="Command preview").grid(row=8, column=0, **padding)
        ttk.Entry(
            self,
            textvariable=self.command_preview_var,
            state="readonly",
        ).grid(row=8, column=1, **padding)

        buttons_frame = ttk.Frame(self)
        buttons_frame.grid(row=9, column=0, columnspan=2, pady=(4, 0))
        self.run_button = ttk.Button(buttons_frame, text="Run clipper", command=self._run_clipper)
        self.run_button.grid(row=0, column=0, padx=4)
        self.stop_button = ttk.Button(
            buttons_frame, text="Stop", command=self._stop_process, state="disabled"
        )
        self.stop_button.grid(row=0, column=1, padx=4)

        ttk.Label(self, text="Output").grid(row=10, column=0, columnspan=2, padx=8, pady=(8, 0), sticky="w")
        self.output_text = scrolledtext.ScrolledText(self, height=18)
        self.output_text.grid(row=11, column=0, columnspan=2, padx=8, pady=(0, 8), sticky="nsew")
        self.rowconfigure(11, weight=1)

    def _wire_variable_listeners(self) -> None:
        self.url_var.trace_add("write", self._on_url_changed)
        for var in (
            self.query_var,
            self.before_var,
            self.after_var,
            self.lang_var,
            self.output_var,
            self.quality_var,
            self.verbose_var,
        ):
            var.trace_add("write", lambda *_args: self._update_command_preview())

    def _maybe_offer_yt_dlp_update(self) -> None:
        if self._offered_update:
            return
        self._offered_update = True
        prompt = (
            "yt-dlp works best when it's up to date (YouTube changes frequently).\n\n"
            "Would you like me to run 'pip install --upgrade yt-dlp' now?"
        )
        if YoutubeDL is None:
            prompt = (
                "yt-dlp is not installed, but it's required for clipping.\n\n"
                "Would you like me to install it now via 'pip install yt-dlp'?"
            )
        if not messagebox.askyesno("Manage yt-dlp", prompt):
            return
        action = "install" if YoutubeDL is None else "upgrade"
        self._start_yt_dlp_update(action)

    def _start_yt_dlp_update(self, action: str) -> None:
        threading.Thread(target=self._run_yt_dlp_update, args=(action,), daemon=True).start()

    def _run_yt_dlp_update(self, action: str) -> None:
        cmd = [sys.executable, "-m", "pip", "install"]
        if action == "upgrade":
            cmd += ["--upgrade", "yt-dlp"]
            human_label = "Updating yt-dlp…"
        else:
            cmd += ["yt-dlp"]
            human_label = "Installing yt-dlp…"
        self.after(0, lambda: self._append_output(f"{human_label}\n$ {' '.join(cmd)}\n"))
        try:
            result = subprocess.run(
                cmd,
                capture_output=True,
                text=True,
            )
        except OSError as exc:
            self.after(
                0,
                lambda: messagebox.showerror("yt-dlp update failed", f"Could not run pip: {exc}"),
            )
            return

        def finalize() -> None:
            output_blocks = []
            if result.stdout:
                output_blocks.append(result.stdout.strip())
            if result.stderr:
                output_blocks.append(result.stderr.strip())
            if output_blocks:
                self._append_output("\n".join(output_blocks) + "\n")
            if result.returncode == 0:
                self._append_output("yt-dlp is ready.\n")
                self._refresh_ytdlp_import()
                messagebox.showinfo("yt-dlp", "yt-dlp is up to date.")
            else:
                messagebox.showerror(
                    "yt-dlp update failed",
                    "pip returned a non-zero exit code. Check the log above for details.",
                )

        self.after(0, finalize)

    def _refresh_ytdlp_import(self) -> None:
        global YoutubeDL
        try:
            module = sys.modules.get("yt_dlp")
            if module is None:
                module = importlib.import_module("yt_dlp")
            else:
                module = importlib.reload(module)
            YoutubeDL = getattr(module, "YoutubeDL", None)
            if YoutubeDL is None:
                raise AttributeError("yt_dlp.YoutubeDL not found after update.")
        except Exception:
            # Leave the old reference in place; errors will surface naturally later.
            pass

    def _on_url_changed(self, *_args: object) -> None:
        self._update_command_preview()
        url = self.url_var.get().strip()
        if self._title_fetch_after_id is not None:
            self.after_cancel(self._title_fetch_after_id)
            self._title_fetch_after_id = None
        if not url:
            self.video_title_var.set("Enter a URL to preview title.")
            self._title_request_counter += 1
            return
        self.video_title_var.set("Fetching title…")
        self._title_fetch_after_id = self.after(600, lambda u=url: self._start_title_lookup(u))

    def _start_title_lookup(self, url: str) -> None:
        self._title_fetch_after_id = None
        self._title_request_counter += 1
        job_id = self._title_request_counter
        threading.Thread(target=self._fetch_title_worker, args=(url, job_id), daemon=True).start()

    def _fetch_title_worker(self, url: str, job_id: int) -> None:
        if YoutubeDL is None:
            message = "Install yt-dlp to preview titles."
        else:
            try:
                with YoutubeDL(
                    {
                        "quiet": True,
                        "no_warnings": True,
                        "skip_download": True,
                    }
                ) as ydl:
                    info = ydl.extract_info(url, download=False)
                title = (info or {}).get("title")
                message = f"Title: {title}" if title else "Title unavailable."
            except Exception as exc:
                message = f"Failed to load title: {exc}"
        self.after(0, lambda: self._apply_title_result(job_id, message))

    def _apply_title_result(self, job_id: int, message: str) -> None:
        if job_id != self._title_request_counter:
            return
        self.video_title_var.set(message)

    def _choose_output(self) -> None:
        selected = filedialog.asksaveasfilename(
            title="Choose output file",
            defaultextension=".mp4",
            filetypes=(("MP4 files", "*.mp4"), ("All files", "*.*")),
        )
        if selected:
            self.output_var.set(selected)

    def _append_output(self, text: str) -> None:
        self.output_text.configure(state="normal")
        self.output_text.insert("end", text)
        self.output_text.see("end")
        self.output_text.configure(state="disabled")

    def _build_command(self, require_required: bool = True) -> list[str]:
        url = self.url_var.get().strip()
        query = self.query_var.get().strip()
        if require_required and (not url or not query):
            raise ValueError("URL and query text are required.")

        cmd: list[str] = [sys.executable, str(self.script_path)]
        if url:
            cmd.append(url)
        if query:
            cmd.append(query)

        before = self._parse_float(self.before_var.get(), "seconds before")
        after = self._parse_float(self.after_var.get(), "seconds after")
        cmd += ["--before", f"{before:.2f}", "--after", f"{after:.2f}"]

        lang = self.lang_var.get().strip()
        if lang:
            cmd += ["--lang", lang]

        output = self.output_var.get().strip()
        if output:
            cmd += ["--output", output]

        quality_label = self.quality_var.get()
        fmt = self.quality_options.get(quality_label, "").strip()
        if fmt:
            cmd += ["--format", fmt]

        if self.verbose_var.get():
            cmd.append("--verbose")
        return cmd

    @staticmethod
    def _parse_float(value: str, field: str) -> float:
        try:
            return float(value)
        except ValueError as exc:
            raise ValueError(f"Invalid value for {field}: '{value or ''}'") from exc

    def _update_command_preview(self) -> None:
        try:
            cmd = self._build_command(require_required=False)
        except ValueError:
            self.command_preview_var.set("Fill in required fields to preview command.")
        else:
            if len(cmd) < 3:
                self.command_preview_var.set("Fill in required fields to preview command.")
            else:
                self.command_preview_var.set(shlex.join(cmd))

    def _run_clipper(self) -> None:
        if self.process is not None:
            messagebox.showinfo("Clipper", "Clipper is already running.")
            return
        try:
            cmd = self._build_command(require_required=True)
        except ValueError as exc:
            messagebox.showerror("Invalid input", str(exc))
            return
        if len(cmd) < 3:
            messagebox.showerror("Missing inputs", "Please fill in the URL and query fields.")
            return

        if not self.script_path.exists():
            messagebox.showerror("Missing script", f"Could not find clipper.py at {self.script_path}")
            return

        self.output_text.configure(state="normal")
        self.output_text.delete("1.0", "end")
        self.output_text.configure(state="disabled")
        self._append_output(f"$ {shlex.join(cmd)}\n\n")
        self.run_button.configure(state="disabled")
        self.stop_button.configure(state="normal")

        try:
            self.process = subprocess.Popen(
                cmd,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                bufsize=1,
            )
        except OSError as exc:
            self.process = None
            self.run_button.configure(state="normal")
            self.stop_button.configure(state="disabled")
            messagebox.showerror("Failed to start", f"Could not launch clipper.py: {exc}")
            return

        threading.Thread(target=self._stream_output, daemon=True).start()

    def _stream_output(self) -> None:
        assert self.process is not None
        with self.process.stdout:
            for line in self.process.stdout:
                self.after(0, self._append_output, line)
        return_code = self.process.wait()
        self.after(
            0,
            lambda: self._on_process_finished(return_code),
        )

    def _on_process_finished(self, return_code: int) -> None:
        self._append_output(f"\nProcess exited with code {return_code}\n")
        self.process = None
        self.run_button.configure(state="normal")
        self.stop_button.configure(state="disabled")
        if return_code == 0:
            messagebox.showinfo("Clipper", "Clip created successfully.")
        else:
            messagebox.showwarning("Clipper", f"Clipper exited with code {return_code}.")

    def _stop_process(self) -> None:
        if self.process is None:
            return
        self.process.terminate()
        self._append_output("\nTerminating clipper process…\n")

    def destroy(self) -> None:
        if self.process is not None:
            try:
                self.process.terminate()
            except OSError:
                pass
        super().destroy()


def main() -> None:
    app = ClipperGUI()
    app.mainloop()


if __name__ == "__main__":
    main()
