# Distribution Setup

Manual one-time steps to enable winget auto-publishing and code signing. Each section is self-contained — do them in any order.

## winget — `WINGET_TOKEN` secret

The `publish-winget` job in `.github/workflows/release.yml` uses the `vedantmgoyal9/winget-releaser` action to fork `microsoft/winget-pkgs`, generate a manifest from the released MSI, and open a PR. It needs a fine-grained personal access token with permission to push to that fork.

1. Go to https://github.com/settings/personal-access-tokens/new
2. **Token name:** `LeopardWM winget releaser`
3. **Expiration:** `1 year` (renew when it expires)
4. **Resource owner:** `jcardama`
5. **Repository access:** `All repositories` (the action creates a fork of microsoft/winget-pkgs on first run; the PAT needs to push to it)
6. **Permissions** → **Repository permissions:**
   - `Contents`: Read and write
   - `Pull requests`: Read and write
   - `Workflows`: Read and write
7. Click **Generate token**, copy the value (starts with `github_pat_`).
8. Add as repo secret:
   ```
   gh secret set WINGET_TOKEN --repo jcardama/LeopardWM
   # (paste the token when prompted)
   ```

The next tagged release that includes the MSI will trigger the action and open a PR to `microsoft/winget-pkgs` automatically.

**Verifying it worked:** after pushing the next `v*` tag, check the `publish-winget` job in the run at https://github.com/jcardama/LeopardWM/actions. On success it logs the PR URL.

---

## Code signing — IDs.cloud verification + Azure Trusted Signing

This eliminates the SmartScreen "Windows protected your PC" prompt on first install. komorebi doesn't have this; GlazeWM does. Without it, every first-time installer hits the prompt and many users abandon.

Two parts: **publisher identity verification** (your part, ~$33, ~1 week async) and **Azure Trusted Signing setup** (we wire into CI once verification clears).

### Step 1 — Identity verification (you do this)

The lowest-cost path for an individual OSS dev:

1. Go to https://www.ids.cloud/identity-verification (or alternative verifier accepted by Azure Trusted Signing — check current list at https://learn.microsoft.com/azure/trusted-signing/concept-trusted-signing-trust-models)
2. Choose **Individual Verification** (~$33). DUNS at $100+ is only needed if signing under a company name.
3. Submit the requested documents (government ID, proof of address). Turnaround is typically 3–7 business days.
4. On approval you'll receive a `MS Identity Validation` confirmation. Save the validation ID.

### Step 2 — Azure Trusted Signing account (you do this; ~10 min after Step 1 clears)

1. Sign in to https://portal.azure.com
2. Search for **Trusted Signing Accounts** → **Create**
3. Pick a region (East US is fine), resource group, and a unique account name like `leopardwm-signing`
4. Tier: **Basic** (~$10/mo)
5. After creation, go to the account → **Identity Validation** → submit the IDs.cloud validation ID from Step 1
6. Once Microsoft accepts (usually within a day), create a **Certificate Profile**:
   - Name: `leopardwm`
   - Type: **Public Trust** → **Individual**
   - Subject: matches your verified identity
7. Note these for the next step:
   - `AZURE_ENDPOINT` (the account's regional endpoint, e.g. `https://eus.codesigning.azure.net`)
   - `AZURE_CODE_SIGNING_NAME` (the account name)
   - `AZURE_CERT_PROFILE_NAME` (the certificate profile name)

### Step 3 — Wire into CI (I do this once you finish Step 2)

When you have the values from Step 2, share them and I'll:

1. Set up an Azure App Registration with federated credentials for GitHub Actions OIDC
2. Add `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_SUBSCRIPTION_ID`, plus the three values above as repo secrets
3. Add an `AzureSignTool` step to `.github/workflows/release.yml` that signs both the MSI and the bundled exes before they're attached to the release
4. Tag a release-candidate (`v0.2.0-rc1`) to verify end-to-end: install the signed MSI on a clean VM and confirm no SmartScreen prompt

---

## Pending: add `lwm.exe` to Scoop manifest

The CLI binary now also installs as `lwm` (short alias). MSI + GitHub-zip already ship both, but the Scoop manifest at `ScoopInstaller/Extras/bucket/leopardwm.json` is still pinned to the two-binary `bin` array because it was submitted before `lwm.exe` existed.

After v0.1.11 (or whichever release first contains `lwm.exe` in the zip) is published, submit a small follow-up PR to ScoopInstaller/Extras adding `"lwm.exe"` to the `bin` array. Excavator (which auto-bumps the version/url/hash) does **not** update `bin` — must be a manual one-line PR.

## Status checklist

- [ ] WINGET_TOKEN secret created (Section 1)
- [ ] IDs.cloud individual verification submitted (Section 2, Step 1)
- [ ] IDs.cloud verification approved (~3–7 days async)
- [ ] Azure Trusted Signing account + cert profile created (Section 2, Step 2)
- [ ] Repo secrets + AzureSignTool step wired (Section 2, Step 3)
- [ ] v0.2.0-rc1 tagged, signing verified on clean VM
