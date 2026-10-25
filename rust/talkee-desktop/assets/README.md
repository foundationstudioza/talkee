# Bundled third-party software

## `VBCABLE_Driver_Pack45.zip`

VB-CABLE, by Vincent Burel / VB-Audio. Talkee ships it and offers to install it, because
Windows has no user-mode way to create a microphone and this is the smallest honest way
around that.

| | |
|---|---|
| Origin | <https://vb-audio.com/Cable/> |
| Downloaded from | `https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack45.zip` |
| Size | 1 318 877 bytes |
| SHA-256 | `B950E39F01AF1D04EA623C8F6D8EB9B6EA5C477C637295FABF20631C85116BFB` |
| Published version | Pack45, October 2024 |

### Why we are allowed to ship it

From <https://vb-audio.com/Services/licensing.htm>, verbatim:

> **[1] We allow to distribute VB-CABLE package (available on www.vb-cable.com) with your
> Application (free or commercial) and possibly embed it in your installation package
> (VB-CABLE silent installation)** if our donationware model is applicable. "Donationware
> model is applicable", means the End User can see and identify VB-CABLE as VB-Audio
> application AND this End User must be in a position to donate/pay license if finding it
> useful. You will have to communicate on this and make it clear for your clients by
> mentioning at least: The origin of VB-CABLE : www.vb-cable.com. VB-CABLE is a
> donationware, all participations are welcome.

The package's own `readme.txt` adds: *"not allowed to integrate the VB-CABLE package in
another software installation procedure without Author agreement"*. The licensing page
**is** that agreement, granted in advance on the three conditions above, which is why
[`setup`](../src/setup.rs) names VB-Audio, calls it donationware, and prints the origin
before it installs anything. If Talkee is ever handed to somebody else, those credits are
not optional — see the last section.

VB-CABLE **A+B and C+D may not be bundled** by anyone. Only this package is here.

### What is inside, and who signed it

The archive holds the driver itself (`.sys`, `.cat` and `.inf` for every supported Windows
version, including ARM64) plus `VBCABLE_Setup.exe`, `VBCABLE_Setup_x64.exe`,
`VBCABLE_ControlPanel.exe` and `readme.txt`.

`VBCABLE_Setup_x64.exe` has a **valid Authenticode signature**:

| | |
|---|---|
| Signer | `CN=BUREL VINCENT Entrepreneur individuel, O=BUREL VINCENT Entrepreneur individuel, S=Dordogne, C=FR`, private organisation, serial 423 734 177 |
| Issuer | `GlobalSign GCC R45 EV CodeSigning CA 2020` |
| Timestamped | 2023-07-11 by `GlobalSign TSA for CodeSign1 - R6 - 202311` |
| Certificate expiry | 2025-06-21 |

The certificate has expired, and the signature is **still valid** — that is what the
timestamp is for. Windows accepts it, which is the thing that matters. Expect anyone who
inspects the installer to ask about it; this paragraph is the answer.

### Installing it

The documented method is: unzip anywhere, run the setup **in administrator mode**, then
**reboot**. The silent switches are not in the readme, but they are confirmed: VB-Audio's
forum has a user automating it with `-i -h` in July 2024 and the author replying about it
without disputing either switch, and the gcloudrig project uses `-i -h` to install and
`-u -h` to uninstall.

What cannot be removed: **the author says the driver-install consent dialog cannot be
suppressed** — "since you install a driver, i don't think it's possible to do it without
asking for user agreement" — and that matches how Windows is designed. So the best possible
first run is one permission prompt and a restart, not none.

### Re-verifying, or moving to a newer pack

```powershell
$zip = 'rust\talkee-desktop\assets\VBCABLE_Driver_Pack45.zip'
(Get-FileHash $zip -Algorithm SHA256).Hash        # must match the table above
Expand-Archive $zip "$env:TEMP\vbc"
Get-AuthenticodeSignature "$env:TEMP\vbc\VBCABLE_Setup_x64.exe" | Format-List Status,SignerCertificate
```

If the hash does not match, **stop**: the archive is not the one that was reviewed, and the
only thing in the repository that installs a kernel driver is now unknown. Replace the file,
update the table, and check the signature again. A newer pack means re-reading the licensing
page too — the terms above are what the current permission rests on.

### If Talkee is ever distributed

The permission is conditional on the End User seeing and being able to pay. That is
satisfied today by [`setup::NOTICE`](../src/setup.rs), which the app prints before installing.
If Talkee gains users who are not the person who built it, the conditions to respect are:

1. Keep naming VB-Audio and VB-CABLE as a donationware, with <https://www.vb-cable.com>.
2. Keep the user able to reach a donation page.
3. For distribution to companies where employees can neither see it nor pay, VB-Audio asks
   that licences be bought on their webshop.
4. If Talkee ever becomes a product with real distribution, VB-Audio "encourage[s]
   distributors ... to make a significant donation as licensing fee", and issues an invoice
   granting the rights. That is cheap, and it is the difference between relying on a general
   permission and holding the specific one.
