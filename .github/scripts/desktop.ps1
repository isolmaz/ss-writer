# Makes the GitHub Windows runner behave like a Windows 11 desktop for the UI tests.
#
# * Short temp paths: the runner's TEMP is the 8.3 form `C:\Users\RUNNER~1\...`, while the shell
#   and file dialogs report long paths, so path comparisons in tests would differ. Point TEMP and
#   TMP at the runner's long temp folder for the following steps.
# * Screen size: the runner's screen is 1024x768 and Windows caps a window at the screen size,
#   but the window tests create windows up to 1350 pixels wide.
# * Animations: the runner has client-area animations off ("best performance"), which makes
#   DirectManipulation jump instead of animate. Windows 11 has them on by default.
$ErrorActionPreference = 'Stop'

$temp = $env:RUNNER_TEMP
"TEMP=$temp" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
"TMP=$temp" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8

Set-DisplayResolution -Width 1920 -Height 1080 -Force

Add-Type -Namespace Win32 -Name Spi -MemberDefinition @'
[DllImport("user32.dll", SetLastError = true)]
public static extern bool SystemParametersInfo(uint action, uint param, IntPtr value, uint flags);
'@
$SPI_SETCLIENTAREAANIMATION = 0x1043
$SPIF_UPDATEINIFILE_SENDCHANGE = 0x3
if (-not [Win32.Spi]::SystemParametersInfo($SPI_SETCLIENTAREAANIMATION, 0, [IntPtr]1, $SPIF_UPDATEINIFILE_SENDCHANGE)) {
    throw "SystemParametersInfo(SPI_SETCLIENTAREAANIMATION) failed"
}

Add-Type -AssemblyName System.Windows.Forms
$bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
"screen $($bounds.Width)x$($bounds.Height), TEMP=$temp, client-area animations on"
