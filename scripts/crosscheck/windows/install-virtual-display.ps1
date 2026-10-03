<#
.SYNOPSIS
Adds a second monitor to a Windows VM with the Virtual Display Driver. Run as an administrator.

.DESCRIPTION
UTM's Windows on Arm VMs have a single display: a second virtio-gpu device stops Windows from
booting. This installs the Virtual Display Driver (github.com/VirtualDrivers/Virtual-Display-Driver,
an Indirect Display Driver signed through SignPath) and creates its root-enumerated device, which is
what devcon's "install" would do, so Windows gets one more monitor of -Width x -Height.

  .\install-virtual-display.ps1 -Width 1920 -Height 1080
#>
param(
    [int] $Width = 1920,
    [int] $Height = 1080,
    [string] $Version = '25.7.23'
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'ARM64' } else { 'x86' }
$work = 'C:\dari-check\vdd'
New-Item -ItemType Directory -Force -Path $work | Out-Null
$zip = "$work\vdd-$Version-$arch.zip"
if (-not (Test-Path $zip)) {
    $url = "https://github.com/VirtualDrivers/Virtual-Display-Driver/releases/download/$Version/VirtualDisplayDriver-$arch.Driver.Only.zip"
    Invoke-WebRequest -Uri $url -OutFile $zip
}
Expand-Archive -Force -Path $zip -DestinationPath $work
$inf = Get-ChildItem -Recurse -Path $work -Filter MttVDD.inf | Select-Object -First 1
foreach ($file in Get-ChildItem -Path $inf.DirectoryName -Include *.cat, *.dll -Recurse) {
    $signature = Get-AuthenticodeSignature $file.FullName
    if ($signature.Status -ne 'Valid') { throw "$($file.Name) is not validly signed: $($signature.Status)" }
}

# Installing a driver from a publisher Windows hasn't seen asks the desktop user to trust it, which
# can't be answered over SSH (0xE0000242). Trusting the catalog's signer up front is what answering
# "Install" there does; the root store is left alone.
$signer = (Get-AuthenticodeSignature (Get-ChildItem -Path $inf.DirectoryName -Filter *.cat | Select-Object -First 1).FullName).SignerCertificate
$publishers = New-Object Security.Cryptography.X509Certificates.X509Store('TrustedPublisher', 'LocalMachine')
$publishers.Open('ReadWrite')
$publishers.Add($signer)
$publishers.Close()

# The driver reads its monitors and modes from here: one monitor that offers only the requested
# mode, so Windows picks it.
New-Item -ItemType Directory -Force -Path C:\VirtualDisplayDriver | Out-Null
[xml] $settings = Get-Content -Raw (Join-Path $inf.DirectoryName 'vdd_settings.xml')
$settings.vdd_settings.monitors.count = '1'
$resolutions = $settings.vdd_settings.resolutions
$resolutions.RemoveAll()
$resolution = $settings.CreateElement('resolution')
foreach ($pair in @(('width', $Width), ('height', $Height), ('refresh_rate', 60))) {
    $element = $settings.CreateElement($pair[0])
    $element.InnerText = [string] $pair[1]
    $resolution.AppendChild($element) | Out-Null
}
$resolutions.AppendChild($resolution) | Out-Null
$settings.Save('C:\VirtualDisplayDriver\vdd_settings.xml')

$hardwareId = 'Root\MttVDD'
$present = Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue |
    Where-Object { $_.HardwareID -contains $hardwareId }
if ($present) {
    pnputil /add-driver $inf.FullName /install | Out-Null
    Write-Output 'The virtual display already exists; driver updated.'
    exit 0
}

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class RootDevice {
    [StructLayout(LayoutKind.Sequential)]
    struct SP_DEVINFO_DATA { public int cbSize; public Guid ClassGuid; public int DevInst; public IntPtr Reserved; }

    [DllImport("setupapi.dll", SetLastError = true)]
    static extern IntPtr SetupDiCreateDeviceInfoList(ref Guid classGuid, IntPtr hwndParent);
    [DllImport("setupapi.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool SetupDiCreateDeviceInfoW(IntPtr set, string name, ref Guid classGuid, string description, IntPtr hwndParent, int flags, ref SP_DEVINFO_DATA data);
    [DllImport("setupapi.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool SetupDiSetDeviceRegistryPropertyW(IntPtr set, ref SP_DEVINFO_DATA data, int property, byte[] buffer, int size);
    [DllImport("setupapi.dll", SetLastError = true)]
    static extern bool SetupDiCallClassInstaller(int function, IntPtr set, ref SP_DEVINFO_DATA data);
    [DllImport("setupapi.dll", SetLastError = true)]
    static extern bool SetupDiDestroyDeviceInfoList(IntPtr set);
    [DllImport("newdev.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool UpdateDriverForPlugAndPlayDevicesW(IntPtr hwndParent, string hardwareId, string infPath, int flags, out bool rebootRequired);

    const int DICD_GENERATE_ID = 1;
    const int SPDRP_HARDWAREID = 1;
    const int DIF_REGISTERDEVICE = 0x19;
    const int INSTALLFLAG_FORCE = 1;

    // What devcon's "install" does: register a root-enumerated device, then install the driver on it.
    public static bool Install(string hardwareId, string infPath) {
        Guid display = new Guid("4d36e968-e325-11ce-bfc1-08002be10318");
        IntPtr set = SetupDiCreateDeviceInfoList(ref display, IntPtr.Zero);
        if (set == new IntPtr(-1)) throw new System.ComponentModel.Win32Exception();
        try {
            SP_DEVINFO_DATA data = new SP_DEVINFO_DATA();
            data.cbSize = Marshal.SizeOf(data);
            if (!SetupDiCreateDeviceInfoW(set, "Display", ref display, null, IntPtr.Zero, DICD_GENERATE_ID, ref data))
                throw new System.ComponentModel.Win32Exception();
            byte[] ids = System.Text.Encoding.Unicode.GetBytes(hardwareId + "\0\0");
            if (!SetupDiSetDeviceRegistryPropertyW(set, ref data, SPDRP_HARDWAREID, ids, ids.Length))
                throw new System.ComponentModel.Win32Exception();
            if (!SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set, ref data))
                throw new System.ComponentModel.Win32Exception();
        } finally {
            SetupDiDestroyDeviceInfoList(set);
        }
        bool reboot;
        if (!UpdateDriverForPlugAndPlayDevicesW(IntPtr.Zero, hardwareId, infPath, INSTALLFLAG_FORCE, out reboot))
            throw new System.ComponentModel.Win32Exception();
        return reboot;
    }
}
'@

$reboot = [RootDevice]::Install($hardwareId, $inf.FullName)
Write-Output "Virtual display installed at ${Width}x${Height}$(if ($reboot) { '; restart to finish' })."
