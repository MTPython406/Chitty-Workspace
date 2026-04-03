# Create a self-signed certificate for local MSIX testing
# Subject must match the Package/Identity/Publisher in AppxManifest.xml

$cert = New-SelfSignedCertificate `
    -Type Custom `
    -Subject "CN=3BC7768D-D3BF-4879-81F1-488F375E8983" `
    -KeyUsage DigitalSignature `
    -FriendlyName "Chitty Workspace Dev Signing" `
    -CertStoreLocation "Cert:\CurrentUser\My" `
    -TextExtension @("2.5.29.37={text}1.3.6.1.5.5.7.3.3", "2.5.29.19={text}")

Write-Host "Certificate created: $($cert.Thumbprint)" -ForegroundColor Green

# Export as PFX for SignTool
$pfxPath = Join-Path $PSScriptRoot "dev-signing.pfx"
$password = ConvertTo-SecureString -String "chittydev" -Force -AsPlainText
Export-PfxCertificate -Cert $cert -FilePath $pfxPath -Password $password | Out-Null
Write-Host "PFX exported: $pfxPath" -ForegroundColor Green

# Install cert as trusted root (needed for MSIX install)
$cerPath = Join-Path $PSScriptRoot "dev-signing.cer"
Export-Certificate -Cert $cert -FilePath $cerPath | Out-Null

# Import to Trusted People store so Windows trusts the MSIX
Import-Certificate -FilePath $cerPath -CertStoreLocation "Cert:\LocalMachine\TrustedPeople" -ErrorAction SilentlyContinue
if ($LASTEXITCODE -ne 0 -and -not $?) {
    Write-Host "Note: Could not auto-trust certificate. You may need to run as admin or manually trust it." -ForegroundColor Yellow
}
Write-Host "Certificate installed to Trusted People store" -ForegroundColor Green

Write-Host ""
Write-Host "Now sign the MSIX with:" -ForegroundColor Cyan
Write-Host "  signtool sign /fd SHA256 /a /f dev-signing.pfx /p chittydev ChittyWorkspace-0.1.0-x64.msix"
