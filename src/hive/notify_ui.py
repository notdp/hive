from __future__ import annotations

import shlex
import subprocess
import tempfile
import textwrap
from pathlib import Path

from . import notify_state
from . import tmux


SWIFT_OVERLAY_SOURCE = r'''#!/usr/bin/env swift

import AppKit
import Foundation

struct Config {
    let message: String
    let agentName: String
    let tabHint: String
    let seconds: Double
    let windowTarget: String
    let paneId: String
    let sessionName: String
    let paneCount: Int
    let paneTitle: String
}

func usage() -> Never {
    FileHandle.standardError.write(Data("Usage: poc_notify_overlay.swift <message> <agent-name> <tab-hint> <seconds> <window-target> <pane-id> <session-name> <pane-count> <pane-title>\n".utf8))
    exit(1)
}

let args = Array(CommandLine.arguments.dropFirst())
guard args.count >= 9 else {
    usage()
}

let message = args[0]
let agentName = args[1]
let tabHint = args[2]
let seconds = Double(args[3]) ?? 6.0
let config = Config(
    message: message,
    agentName: agentName,
    tabHint: tabHint,
    seconds: max(1.0, seconds),
    windowTarget: args[4],
    paneId: args[5],
    sessionName: args[6],
    paneCount: Int(args[7]) ?? 1,
    paneTitle: args[8]
)

final class OverlayPanel: NSPanel {
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { true }
}

func runAppleScript(_ source: String) {
    let process = Process()
    process.executableURL = URL(fileURLWithPath: "/usr/bin/osascript")
    process.arguments = ["-"] + args
    let stdin = Pipe()
    process.standardInput = stdin
    process.standardOutput = Pipe()
    process.standardError = Pipe()
    do {
        try process.run()
        stdin.fileHandleForWriting.write(Data(source.utf8))
        stdin.fileHandleForWriting.closeFile()
        process.waitUntilExit()
    } catch {
    }
}

func focusTarget(windowTarget: String, sessionName: String, paneId: String, tabHint: String, paneCount: Int, paneTitle: String) {
    let escapedWindowTarget = windowTarget.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\"")
    let escapedSessionName = sessionName.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\"")
    let escapedPaneId = paneId.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\"")
    let escapedTabHint = tabHint.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\"")
    let escapedPaneTitle = paneTitle.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\"")

    let script = """
tell application id \"com.googlecode.iterm2\"
  activate
  tell first window
    set targetIndex to 1
    set desiredTabHint to \"\(escapedTabHint)\"
    set desiredPaneCount to \(paneCount)
    set desiredPaneTitle to \"\(escapedPaneTitle)\"
    set rank to 0
    repeat with tabRef in tabs
      set rank to rank + 1
      set tabSessionCount to count of sessions of tabRef
      try
        set tabTitle to name of current session of tabRef
      on error
        set tabTitle to \"\"
      end try
      if tabSessionCount is desiredPaneCount then
        if desiredPaneCount > 1 then
          set targetIndex to rank
          exit repeat
        else if desiredPaneTitle is not \"\" and tabTitle contains desiredPaneTitle then
          set targetIndex to rank
          exit repeat
        end if
      end if
      if desiredTabHint is not \"\" and tabTitle contains desiredTabHint then
        set targetIndex to rank
        exit repeat
      else if tabTitle contains \"\(escapedSessionName)\" then
        set targetIndex to rank
      end if
    end repeat
    try
      select (tab targetIndex)
    end try
  end tell
end tell
do shell script \"TMUX= tmux select-window -t \\\"\(escapedWindowTarget)\\\" >/dev/null 2>&1; TMUX= tmux select-pane -t \\\"\(escapedPaneId)\\\" >/dev/null 2>&1\"
"""

    runAppleScript(script)
}

func closeOverlay(after delay: Double, panel: NSPanel) {
    DispatchQueue.main.asyncAfter(deadline: .now() + delay) {
        NSAnimationContext.runAnimationGroup({ context in
            context.duration = 0.2
            panel.animator().alphaValue = 0
        }, completionHandler: {
            NSApp.terminate(nil)
        })
    }
}

final class OverlayView: NSVisualEffectView {
    var onTab: (() -> Void)?
    var onEscape: (() -> Void)?

    override var acceptsFirstResponder: Bool { true }

    override func keyDown(with event: NSEvent) {
        switch event.keyCode {
        case 48:
            onTab?()
        case 53:
            onEscape?()
        default:
            super.keyDown(with: event)
        }
    }

    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        if event.keyCode == 48 {
            onTab?()
            return true
        }
        if event.keyCode == 53 {
            onEscape?()
            return true
        }
        return super.performKeyEquivalent(with: event)
    }

    override func cancelOperation(_ sender: Any?) {
        onEscape?()
    }

    override func insertTab(_ sender: Any?) {
        onTab?()
    }
}

let app = NSApplication.shared
app.setActivationPolicy(.accessory)

func monoFont(_ size: CGFloat, _ weight: NSFont.Weight = .regular) -> NSFont {
    let base = NSFont(name: "JetBrains Mono", size: size) ?? NSFont(name: "Menlo", size: size) ?? NSFont.monospacedSystemFont(ofSize: size, weight: weight)
    return base
}

let sol_base03  = NSColor(red: 0.027, green: 0.212, blue: 0.259, alpha: 1)    // #073642
let sol_base00  = NSColor(red: 0.396, green: 0.482, blue: 0.514, alpha: 1)    // #657B83
let sol_base0   = NSColor(red: 0.576, green: 0.631, blue: 0.631, alpha: 1)    // #93A1A1
let sol_base2   = NSColor(red: 0.933, green: 0.910, blue: 0.835, alpha: 1)    // #EEE8D5
let sol_base3   = NSColor(red: 0.992, green: 0.965, blue: 0.890, alpha: 1)    // #FDF6E3
let sol_green   = NSColor(red: 0.522, green: 0.600, blue: 0.000, alpha: 1)    // #859900

let screenFrame = NSScreen.main?.visibleFrame ?? NSScreen.screens.first?.visibleFrame ?? NSRect(x: 0, y: 0, width: 1440, height: 900)

let titleBarH: CGFloat = 27
let bodyTopPad: CGFloat = 14
let bodyBottomPad: CGFloat = 10
let bodyPadH: CGFloat = 16
let lineGap: CGFloat = 12
let statusH: CGFloat = 16
let dividerH: CGFloat = 1
let keysH: CGFloat = 17
let hasHint = !config.tabHint.isEmpty
let messageWidth = 460 - bodyPadH * 2
let messageRect = NSRect(x: 0, y: 0, width: messageWidth, height: 200)
let messageAttrs: [NSAttributedString.Key: Any] = [.font: monoFont(17, .semibold)]
let measuredMessage = ("$ " + config.message) as NSString
let measuredMessageBounds = measuredMessage.boundingRect(with: messageRect.size, options: [.usesLineFragmentOrigin, .usesFontLeading], attributes: messageAttrs)
let clampedMessageHeight = min(max(25, ceil(measuredMessageBounds.height)), 75)
let msgH: CGFloat = clampedMessageHeight
let bodyH = bodyTopPad + msgH + lineGap + statusH + lineGap + dividerH + lineGap + keysH + bodyBottomPad
let panelH = titleBarH + bodyH
let panelSize = NSSize(width: 460, height: panelH)
let panelOrigin = NSPoint(x: screenFrame.midX - panelSize.width / 2, y: screenFrame.midY - panelSize.height / 2)

let panel = OverlayPanel(contentRect: NSRect(origin: panelOrigin, size: panelSize), styleMask: [.borderless], backing: .buffered, defer: false)
panel.level = .statusBar
panel.isFloatingPanel = true
panel.hidesOnDeactivate = false
panel.backgroundColor = .clear
panel.isOpaque = false
panel.hasShadow = true
panel.ignoresMouseEvents = false
panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .transient, .ignoresCycle]

let contentView = OverlayView(frame: NSRect(origin: .zero, size: panelSize))
contentView.material = .popover
contentView.blendingMode = .behindWindow
contentView.state = .active
contentView.wantsLayer = true
contentView.layer?.cornerRadius = 8
contentView.layer?.masksToBounds = true
contentView.layer?.borderWidth = 1
contentView.layer?.borderColor = sol_base0.withAlphaComponent(0.2).cgColor
panel.contentView = contentView

func handleOverlayKey(_ event: NSEvent) -> Bool {
    switch event.keyCode {
    case 48:
        contentView.onTab?()
        return true
    case 53:
        contentView.onEscape?()
        return true
    default:
        return false
    }
}

let keyMonitor = NSEvent.addLocalMonitorForEvents(matching: [.keyDown]) { event in
    if handleOverlayKey(event) {
        return nil
    }
    return event
}

func dismissOverlay(_ panel: NSPanel) {
    if let monitor = keyMonitor {
        NSEvent.removeMonitor(monitor)
    }
    panel.orderOut(nil)
    NSApp.terminate(nil)
}

let bgOverlay = NSView(frame: NSRect(x: 0, y: 0, width: panelSize.width, height: panelSize.height - titleBarH))
bgOverlay.wantsLayer = true
bgOverlay.layer?.backgroundColor = sol_base3.withAlphaComponent(0.9).cgColor
contentView.addSubview(bgOverlay)

func makeLabel(text: String, font: NSFont, color: NSColor, frame: NSRect, alignment: NSTextAlignment = .left) -> NSTextField {
    let label = NSTextField(labelWithString: text)
    label.frame = frame
    label.font = font
    label.textColor = color
    label.alignment = alignment
    label.lineBreakMode = .byWordWrapping
    label.maximumNumberOfLines = 0
    label.backgroundColor = .clear
    return label
}

func truncated(_ text: String, font: NSFont, width: CGFloat) -> String {
    let source = text as NSString
    if source.size(withAttributes: [.font: font]).width <= width {
        return text
    }
    let ellipsis = "…" as NSString
    var low = 0
    var high = source.length
    while low < high {
        let mid = (low + high + 1) / 2
        let candidate = source.substring(to: mid) + "…"
        let candidateWidth = (candidate as NSString).size(withAttributes: [.font: font]).width
        if candidateWidth <= width {
            low = mid
        } else {
            high = mid - 1
        }
    }
    if low <= 0 {
        return ellipsis as String
    }
    return source.substring(to: low) + "…"
}

let titleBar = NSView(frame: NSRect(x: 0, y: panelSize.height - titleBarH, width: panelSize.width, height: titleBarH))
titleBar.wantsLayer = true
titleBar.layer?.backgroundColor = sol_base2.cgColor
let titleBorder = CALayer()
titleBorder.frame = CGRect(x: 0, y: 0, width: panelSize.width, height: 1)
titleBorder.backgroundColor = sol_base0.withAlphaComponent(0.22).cgColor
titleBar.layer?.addSublayer(titleBorder)
let userName = NSUserName()
let rawTitleText = "\(userName)@hive:~ notify\(hasHint ? " — \(config.tabHint)" : "")"
let titleText = truncated(rawTitleText, font: monoFont(11), width: panelSize.width - 24)
let titleLabel = makeLabel(text: titleText, font: monoFont(11), color: sol_base0, frame: NSRect(x: 12, y: 6, width: panelSize.width - 24, height: 15), alignment: .left)
titleBar.addSubview(titleLabel)
contentView.addSubview(titleBar)

func sizedLabel(_ text: String, _ font: NSFont, _ color: NSColor) -> NSTextField {
    let label = NSTextField(labelWithString: text)
    label.font = font
    label.textColor = color
    label.backgroundColor = .clear
    label.isBordered = false
    label.isEditable = false
    label.sizeToFit()
    return label
}

let gap: CGFloat = lineGap
var curY = panelSize.height - titleBarH - bodyTopPad

let msgLabel = sizedLabel("$ \(config.message)", monoFont(17, .semibold), sol_base03)
msgLabel.lineBreakMode = .byTruncatingTail
msgLabel.maximumNumberOfLines = 3
msgLabel.frame = NSRect(x: bodyPadH, y: curY - msgH, width: panelSize.width - bodyPadH * 2, height: msgH)
contentView.addSubview(msgLabel)
curY -= msgH + gap

let statusFont = monoFont(12)
let statusFontBold = monoFont(12, .semibold)
let rowH = ceil(statusFont.boundingRectForFont.height)

let dotSize: CGFloat = 6
let dotY = curY - rowH / 2 - dotSize / 2
let dotView = NSView(frame: NSRect(x: bodyPadH, y: dotY, width: dotSize, height: dotSize))
dotView.wantsLayer = true
dotView.layer?.cornerRadius = dotSize / 2
dotView.layer?.backgroundColor = sol_green.cgColor
let glowLayer = CALayer()
glowLayer.frame = dotView.bounds.insetBy(dx: -2, dy: -2)
glowLayer.cornerRadius = (dotSize + 4) / 2
glowLayer.backgroundColor = sol_green.withAlphaComponent(0.25).cgColor
dotView.layer?.insertSublayer(glowLayer, at: 0)
contentView.addSubview(dotView)

var sx = bodyPadH + dotSize + 6
let roleText = config.agentName.isEmpty ? "notify" : config.agentName
let roleLabel = sizedLabel(roleText, statusFontBold, sol_green)
roleLabel.frame.origin = NSPoint(x: sx, y: curY - rowH)
roleLabel.frame.size.height = rowH
contentView.addSubview(roleLabel)
sx = roleLabel.frame.maxX + 6

let pipeLabel = sizedLabel("│", statusFont, sol_base00.withAlphaComponent(0.27))
pipeLabel.frame.origin = NSPoint(x: sx, y: curY - rowH)
pipeLabel.frame.size.height = rowH
contentView.addSubview(pipeLabel)
sx = pipeLabel.frame.maxX + 6

let tabInfoStr = config.tabHint.isEmpty ? config.sessionName : config.tabHint
let tabInfoLabel = sizedLabel(tabInfoStr, statusFont, sol_base00.withAlphaComponent(0.6))
tabInfoLabel.lineBreakMode = .byTruncatingTail
tabInfoLabel.frame = NSRect(x: sx, y: curY - rowH, width: panelSize.width - sx - bodyPadH, height: rowH)
contentView.addSubview(tabInfoLabel)
curY -= rowH + gap

let divider = NSView(frame: NSRect(x: bodyPadH, y: curY - 1, width: panelSize.width - bodyPadH * 2, height: 1))
divider.wantsLayer = true
divider.layer?.backgroundColor = sol_base0.withAlphaComponent(0.15).cgColor
contentView.addSubview(divider)
curY -= 1 + gap

let keyFont = monoFont(10, .medium)
let keyDescFont = monoFont(11)
let keyColor = sol_base00.withAlphaComponent(0.7)
let descColor = sol_base00.withAlphaComponent(0.5)

func makeKeyBadge(_ text: String, x: CGFloat, baseline: CGFloat) -> (badge: NSView, right: CGFloat) {
    let label = sizedLabel(text, keyFont, keyColor)
    let padH: CGFloat = 5
    let padV: CGFloat = 2
    let badgeW = label.frame.width + padH * 2
    let badgeH = label.frame.height + padV * 2
    let badgeY = baseline - badgeH / 2
    let badge = NSView(frame: NSRect(x: x, y: badgeY, width: badgeW, height: badgeH))
    badge.wantsLayer = true
    badge.layer?.cornerRadius = 3
    badge.layer?.backgroundColor = sol_base2.cgColor
    badge.layer?.borderWidth = 1
    badge.layer?.borderColor = sol_base0.withAlphaComponent(0.2).cgColor
    label.frame.origin = NSPoint(x: padH, y: padV)
    badge.addSubview(label)
    return (badge, badge.frame.maxX)
}

let descH = ceil(keyDescFont.boundingRectForFont.height)
let badgeMidY = curY - descH / 2

let (tabBadge, tabBadgeR) = makeKeyBadge("tab", x: bodyPadH, baseline: badgeMidY)
contentView.addSubview(tabBadge)
let tabDescLabel = sizedLabel("focus pane", keyDescFont, descColor)
tabDescLabel.frame.origin = NSPoint(x: tabBadgeR + 5, y: curY - descH)
tabDescLabel.frame.size.height = descH
contentView.addSubview(tabDescLabel)

let (escBadge, escBadgeR) = makeKeyBadge("esc", x: tabDescLabel.frame.maxX + 14, baseline: badgeMidY)
contentView.addSubview(escBadge)
let escDescLabel = sizedLabel("dismiss", keyDescFont, descColor)
escDescLabel.frame.origin = NSPoint(x: escBadgeR + 5, y: curY - descH)
escDescLabel.frame.size.height = descH
contentView.addSubview(escDescLabel)

contentView.onTab = {
    focusTarget(
        windowTarget: config.windowTarget,
        sessionName: config.sessionName,
        paneId: config.paneId,
        tabHint: config.tabHint,
        paneCount: config.paneCount,
        paneTitle: config.paneTitle
    )
    dismissOverlay(panel)
}

contentView.onEscape = {
    dismissOverlay(panel)
}

panel.alphaValue = 0
panel.orderFrontRegardless()
NSApp.activate(ignoringOtherApps: true)
panel.makeKeyAndOrderFront(nil)
panel.orderFrontRegardless()
panel.makeFirstResponder(contentView)
DispatchQueue.main.async {
    panel.makeKey()
    panel.makeMain()
    panel.makeFirstResponder(contentView)
}

NSAnimationContext.runAnimationGroup { context in
    context.duration = 0.12
    panel.animator().alphaValue = 1
}

closeOverlay(after: config.seconds, panel: panel)

app.run()
'''


TMUX_POPUP_SOURCE = r'''#!/usr/bin/env bash
set -euo pipefail

script_path="$0"
cleanup() {
  printf '\033[0m\033[?25h'
  rm -f "$script_path"
}
trap cleanup EXIT

message="$1"
window_target="$2"
pane_id="$3"
tab_hint="$4"
agent_name="$5"
seconds="$6"
content_width="$7"

accent=$'\033[38;5;65m'
strong=$'\033[1;38;5;235m'
muted=$'\033[38;5;241m'
soft=$'\033[38;5;109m'
reset=$'\033[0m'

render_footer() {
  local remaining="$1"
  printf '%s[Space]%s jump   %s[Any key]%s dismiss   %s[%ss]%s close' "$accent" "$reset" "$accent" "$reset" "$accent" "$remaining" "$reset"
}

box_line="$(printf '%*s' "$((content_width + 2))" '' | tr ' ' '─')"

printf '\033[48;5;230m\033[38;5;235m\033[2J\033[H\033[?25l'
printf '%s%s notify %s\n' "$accent" "$strong" "$reset"
printf '%s════════════════════════════════════════════════════════════%s\n' "$accent" "$reset"
printf '%s┌%s┐%s\n' "$soft" "$box_line" "$reset"
while IFS= read -r line; do
  printf '%s│%s %-*s %s│%s\n' "$soft" "$reset" "$content_width" "$line" "$soft" "$reset"
done < <(printf '%s\n' "$message" | fold -s -w "$content_width")
printf '%s└%s┘%s\n\n' "$soft" "$box_line" "$reset"
printf '%sAgent:%s  %s%s%s\n' "$muted" "$reset" "$strong" "${agent_name:-notify}" "$reset"
printf '%sWindow:%s %s%s%s\n' "$muted" "$reset" "$strong" "${tab_hint:-$window_target}" "$reset"
printf '%sPane:%s   %s%s%s\n\n' "$muted" "$reset" "$strong" "$pane_id" "$reset"
render_footer "$seconds"

key=""
remaining="$seconds"
while (( remaining > 0 )); do
  if IFS= read -rsn1 -t 1 key; then
    if [[ "$key" == ' ' ]]; then
      TMUX= tmux select-window -t "$window_target" >/dev/null 2>&1 || true
      TMUX= tmux select-pane -t "$pane_id" >/dev/null 2>&1 || true
    fi
    break
  fi
  remaining=$((remaining - 1))
  if (( remaining > 0 )); then
    printf '\r\033[2K'
    render_footer "$remaining"
  fi
done
'''


def _write_temp_swift() -> Path:
    handle = tempfile.NamedTemporaryFile("w", suffix=".swift", delete=False)
    with handle:
        handle.write(SWIFT_OVERLAY_SOURCE)
    return Path(handle.name)


def _write_temp_popup_script() -> Path:
    handle = tempfile.NamedTemporaryFile("w", suffix=".sh", delete=False)
    with handle:
        handle.write(TMUX_POPUP_SOURCE)
    path = Path(handle.name)
    path.chmod(0o755)
    return path


def _popup_geometry(message: str, *, window_name: str, agent_name: str, pane_id: str, seconds: int) -> tuple[int, int, int]:
    min_width = 56
    max_width = 72
    chrome_width = 8
    footer = f"[Space] jump   [Any key] dismiss   [{max(1, seconds)}s] close"
    candidates = ([len(line) for line in message.splitlines()] or [0]) + [
        len(window_name),
        len(agent_name or "notify"),
        len(pane_id),
        len(footer),
    ]
    longest = max(candidates)
    width = max(min_width, min(max_width, longest + chrome_width))
    content_width = max(38, width - 8)

    wrapped_lines = 0
    for line in message.splitlines() or [message]:
        wrapped = textwrap.wrap(line, width=content_width) or [""]
        wrapped_lines += len(wrapped)
    height = min(18, max(11, wrapped_lines + 7))
    return width, height, content_width


def _popup_position(pane_id: str, popup_width: int) -> tuple[str, str]:
    window_width_raw = tmux.display_value(pane_id, "#{window_width}")
    try:
        window_width = int(window_width_raw or "0")
    except ValueError:
        window_width = 0
    if window_width <= 0:
        return "C", "C"
    x = max(0, window_width - popup_width - 1)
    return str(x), "1"



def show_overlay(message: str, pane_id: str, seconds: int = 12) -> None:
    script_path = _write_temp_swift()
    window_name = tmux.get_pane_window_name(pane_id) or "target"
    agent_name = tmux.get_pane_option(pane_id, "hive-agent") or ""
    args = [
        "swift",
        str(script_path),
        message,
        agent_name,
        window_name,
        str(max(1, seconds)),
        tmux.get_pane_window_target(pane_id) or "",
        pane_id,
        tmux.get_pane_session_name(pane_id) or "",
        str(tmux.get_pane_count(pane_id)),
        tmux.get_pane_title(pane_id) or "",
    ]
    subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def show_tmux_popup(message: str, pane_id: str, seconds: int = 12) -> None:
    script_path = _write_temp_popup_script()
    window_target = tmux.get_pane_window_target(pane_id) or ""
    window_name = tmux.get_pane_window_name(pane_id) or "target"
    agent_name = tmux.get_pane_option(pane_id, "hive-agent") or ""
    popup_width, popup_height, content_width = _popup_geometry(
        message,
        window_name=window_name,
        agent_name=agent_name,
        pane_id=pane_id,
        seconds=seconds,
    )
    popup_x, popup_y = _popup_position(pane_id, popup_width)
    args = [
        "bash",
        str(script_path),
        message,
        window_target,
        pane_id,
        window_name,
        agent_name,
        str(max(1, seconds)),
        str(content_width),
    ]
    popup_cmd = " ".join(shlex.quote(arg) for arg in args)
    subprocess.run([
        "tmux",
        "display-popup",
        "-t",
        pane_id,
        "-x",
        popup_x,
        "-y",
        popup_y,
        "-w",
        str(popup_width),
        "-h",
        str(popup_height),
        "-s",
        "fg=colour235,bg=colour230",
        "-S",
        "fg=colour65,bold",
        "-T",
        " notify ",
        "-E",
        popup_cmd,
    ], check=False, capture_output=True, text=True)


def _user_is_already_in_target_window(pane_id: str, *, session_name: str, window_target: str) -> bool:
    if not session_name or not window_target:
        return False
    active_window = tmux.get_most_recent_client_window(session_name)
    return bool(active_window and active_window == window_target)


def notify(
    message: str,
    pane_id: str,
    seconds: int = 12,
    *,
    highlight: bool = False,
    window_status: bool = True,
    source: str = notify_state.SOURCE_AGENT_CLI,
    kind: str = "agent_attention",
) -> dict[str, object]:
    window_target = tmux.get_pane_window_target(pane_id) or ""
    window_name = tmux.get_pane_window_name(pane_id) or "target"
    agent_name = tmux.get_pane_option(pane_id, "hive-agent") or ""
    session_name = tmux.get_pane_session_name(pane_id) or ""
    client_mode = tmux.get_client_mode(pane_id)
    suppressed = _user_is_already_in_target_window(
        pane_id,
        session_name=session_name,
        window_target=window_target,
    )
    if suppressed:
        return {
            "agent": agent_name,
            "paneId": pane_id,
            "window": window_target,
            "tab": window_name,
            "message": message,
            "seconds": seconds,
            "clientMode": client_mode,
            "surface": "suppressed",
            "highlight": highlight,
            "windowStatus": window_status,
            "suppressed": True,
            "suppressionReason": "same_window",
        }

    notify_state.record_notification(pane_id, source=source, kind=kind, message=message)
    if highlight:
        tmux.flash_pane_border(pane_id, seconds=seconds)
    if window_status and window_target:
        tmux.flash_window_status(window_target, seconds=seconds)
    surface = "overlay"
    if client_mode == "terminal" and tmux.supports_popup():
        show_tmux_popup(message, pane_id, seconds=seconds)
        surface = "popup"
    else:
        show_overlay(message, pane_id, seconds=seconds)
    return {
        "agent": agent_name,
        "paneId": pane_id,
        "window": window_target,
        "tab": window_name,
        "message": message,
        "seconds": seconds,
        "clientMode": client_mode,
        "surface": surface,
        "highlight": highlight,
        "windowStatus": window_status,
        "suppressed": False,
    }
