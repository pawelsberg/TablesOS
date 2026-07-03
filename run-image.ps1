#requires -Version 5
<#
.SYNOPSIS
    Boot the already-built TablesOS image in QEMU WITHOUT rebuilding it.

.DESCRIPTION
    `cargo run` regenerates target\tablesos.img from scratch every time, which
    wipes any data the running OS persisted into it. This script launches QEMU on
    the existing image instead, so your data survives across reboots. It is the
    same QEMU invocation the builder uses (see src/main.rs), minus the build step.

    Your data lives INSIDE the image file. To keep it: use this script, and do
    NOT run `cargo run` (or back the image up first — see -Image).

.PARAMETER Image
    Image to boot. Defaults to target\tablesos.img. Point this at a backup copy
    (e.g. target\tablesos.save.img) to resume from a snapshot.

.PARAMETER Uefi
    Boot through OVMF/EDK2 firmware (the BOOTX64.EFI path) instead of legacy BIOS.

.PARAMETER Qemu
    Path to qemu-system-x86_64.exe. Defaults to the QEMU env var, then the
    standard install dir, then PATH.

.EXAMPLE
    .\run-image.ps1
    Boot target\tablesos.img via BIOS, no rebuild.

.EXAMPLE
    .\run-image.ps1 -Uefi
    Boot the same image through UEFI firmware.

.EXAMPLE
    .\run-image.ps1 -Image target\tablesos.save.img
    Resume from a backup copy you made before rebuilding.
#>
[CmdletBinding()]
param(
    [string]$Image,
    [switch]$Uefi,
    [string]$Qemu
)

$ErrorActionPreference = 'Stop'
$root   = $PSScriptRoot
$target = Join-Path $root 'target'

# ---- resolve the image -------------------------------------------------------
if (-not $Image) { $Image = Join-Path $target 'tablesos.img' }
if (-not [IO.Path]::IsPathRooted($Image)) { $Image = Join-Path $root $Image }
if (-not (Test-Path $Image)) {
    throw "Image not found: $Image`n" +
          "Build it once with 'cargo run -- --no-run' (add --seed for sample data), then re-run this script."
}

# ---- locate QEMU -------------------------------------------------------------
if (-not $Qemu) {
    if ($env:QEMU) {
        $Qemu = $env:QEMU
    } elseif (Test-Path 'C:\Program Files\qemu\qemu-system-x86_64.exe') {
        $Qemu = 'C:\Program Files\qemu\qemu-system-x86_64.exe'
    } else {
        $cmd = Get-Command qemu-system-x86_64.exe -ErrorAction SilentlyContinue
        if ($cmd) { $Qemu = $cmd.Source }
    }
}
if (-not $Qemu -or -not (Test-Path $Qemu)) {
    throw "QEMU not found. Pass -Qemu <path-to-qemu-system-x86_64.exe> or set the QEMU env var."
}

# ---- USB mass-storage scratch disk the kernel enumerates on the xHCI bus -----
# Not part of the boot image; a blank 64 MiB disk is enough to exercise the USB
# path (and to be an install/upgrade target). Created if absent.
$usb = Join-Path $target 'usbstick.img'
if (-not (Test-Path $usb)) {
    Write-Host "Creating blank 64 MiB scratch USB disk: $usb"
    $fs = [IO.File]::Create($usb)
    try { $fs.SetLength(64MB) } finally { $fs.Close() }
}

# ---- assemble the QEMU command (mirrors src/main.rs run path) -----------------
$qargs = @(
    '-machine', 'pc,accel=whpx:tcg,kernel-irqchip=off'
    '-m', '1024M'
    '-drive', "format=raw,file=$Image,if=ide,index=0,media=disk"
    '-device', 'qemu-xhci,id=xhci'
    '-drive', "if=none,id=usbstick,format=raw,file=$usb"
    '-device', 'usb-storage,bus=xhci.0,drive=usbstick'
    '-device', 'usb-mouse,bus=xhci.0'
    '-serial', 'stdio'
    '-vga', 'std'
    '-no-reboot'
)

if ($Uefi) {
    $share   = Join-Path (Split-Path $Qemu -Parent) 'share'
    $code    = Join-Path $share 'edk2-x86_64-code.fd'
    $varsSrc = Join-Path $share 'edk2-i386-vars.fd'
    $vars    = Join-Path $target 'uefi-vars.fd'
    if (-not (Test-Path $code)) {
        throw "UEFI firmware not found at $code.`n" +
              "Use a QEMU build that ships EDK2, or drop the -Uefi switch to boot via BIOS."
    }
    if (-not (Test-Path $vars)) { Copy-Item $varsSrc $vars }
    # pflash drives go first, matching the builder's UEFI invocation.
    $qargs = @(
        '-drive', "if=pflash,format=raw,readonly=on,file=$code"
        '-drive', "if=pflash,format=raw,file=$vars"
    ) + $qargs
}

# ---- launch ------------------------------------------------------------------
$mode = if ($Uefi) { 'UEFI' } else { 'BIOS' }
Write-Host ""
Write-Host "Booting (no rebuild): $Image" -ForegroundColor Cyan
Write-Host "  firmware: $mode   qemu: $Qemu"
Write-Host "  The OS persists data INTO this image file. To keep it, do NOT run 'cargo run'." -ForegroundColor Yellow
Write-Host "  (Boot trace renders in the QEMU window, not this console.)"
Write-Host ""

& $Qemu @qargs
exit $LASTEXITCODE
