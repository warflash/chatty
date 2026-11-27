# End-to-end check without a microphone or the mouse.
#
#   $env:CHATTY_TEST_WAV = 'path\to\speech.wav'   # replayed as the microphone
#   $env:CHATTY_ACCEPT_INJECTED = '1'             # hook accepts these keys
#   target\release\chatty.exe
#   ./scripts/e2e.ps1 -HoldMs 12000                # dictate into the focused window
#   ./scripts/e2e.ps1 -HoldMs 80                   # tap: Enter
#
# Sends Ctrl+Numpad / exactly as the mouse does: Ctrl down, Divide down
# (extended key, repeating while held), Divide up, Ctrl up.
param(
    [int]$HoldMs = 80,
    [int]$RepeatMs = 33,
    [switch]$Escape
)
Add-Type -Namespace E2E -Name Keys -MemberDefinition @'
[DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, System.UIntPtr extra);
'@
$CTRL = 0x11; $DIVIDE = 0x6F; $ESC = 0x1B
$EXT = 0x1; $UP = 0x2
[E2E.Keys]::keybd_event($CTRL, 0x1D, 0, [UIntPtr]::Zero)
[E2E.Keys]::keybd_event($DIVIDE, 0x35, $EXT, [UIntPtr]::Zero)
$sw = [Diagnostics.Stopwatch]::StartNew()
# Windows auto-repeat starts after ~500 ms.
Start-Sleep -Milliseconds ([Math]::Min($HoldMs, 500))
while ($sw.ElapsedMilliseconds -lt $HoldMs) {
    [E2E.Keys]::keybd_event($DIVIDE, 0x35, $EXT, [UIntPtr]::Zero)
    if ($Escape -and $sw.ElapsedMilliseconds -gt $HoldMs / 2) {
        [E2E.Keys]::keybd_event($ESC, 0x01, 0, [UIntPtr]::Zero)
        [E2E.Keys]::keybd_event($ESC, 0x01, $UP, [UIntPtr]::Zero)
        $Escape = $false
    }
    Start-Sleep -Milliseconds $RepeatMs
}
[E2E.Keys]::keybd_event($DIVIDE, 0x35, $EXT -bor $UP, [UIntPtr]::Zero)
[E2E.Keys]::keybd_event($CTRL, 0x1D, $UP, [UIntPtr]::Zero)
Write-Host "released after $($sw.ElapsedMilliseconds) ms"
