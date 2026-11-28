// Per-executable Windows Filtering Platform block, including local loopback.
// Build: gcc -O2 -Wall -Wextra -municode -o victory-offline-wfp.exe victory-offline-wfp.c -lfwpuclnt -ladvapi32 -luuid
#define WIN32_LEAN_AND_MEAN
#define _WIN32_WINNT 0x0601
#include <windows.h>
#include <fwpmu.h>
#include <fwpmtypes.h>
#include <fwptypes.h>
#include <wincrypt.h>
#include <stdio.h>
#include <wchar.h>

#ifndef FWPM_FILTER_FLAG_PERSISTENT
#define FWPM_FILTER_FLAG_PERSISTENT 0x00000001
#endif
#ifndef FWPM_SUBLAYER_FLAG_PERSISTENT
#define FWPM_SUBLAYER_FLAG_PERSISTENT 0x00000001
#endif

#define FILTER_NOT_FOUND 0x80320003u
#define SUBLAYER_NOT_FOUND 0x80320007u
#define ALREADY_EXISTS 0x80320009u

static const GUID VICTORY_SUBLAYER = {0x5ad61342,0xd4ab,0x4cd4,{0x9e,0xa0,0xe3,0x4a,0xf5,0x2c,0xf4,0xb2}};
// Public WFP identifiers from Windows SDK fwpmu.h (MinGW does not declare them).
static const GUID FWPM_LAYER_ALE_AUTH_CONNECT_V4 = {0xc38d57d1,0x05a7,0x4c33,{0x90,0x4f,0x7f,0xbc,0xee,0xe6,0x0e,0x82}};
static const GUID FWPM_LAYER_ALE_AUTH_CONNECT_V6 = {0x4a72393b,0x319f,0x44bc,{0x84,0xc3,0xba,0x54,0xdc,0xb3,0xb6,0xb4}};
static const GUID FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V4 = {0xe1cd9fe7,0xf4b5,0x4273,{0x96,0xc0,0x59,0x2e,0x48,0x7b,0x86,0x50}};
static const GUID FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V6 = {0xa3b42c97,0x9f04,0x4672,{0xb8,0x7e,0xce,0xe9,0xc4,0x83,0x25,0x7f}};
static const GUID FWPM_CONDITION_ALE_APP_ID = {0xd78e1e87,0x8644,0x4ea5,{0x94,0x37,0xd8,0x09,0xec,0xef,0xc9,0x71}};
static const GUID *LAYERS[4] = {
    &FWPM_LAYER_ALE_AUTH_CONNECT_V4,
    &FWPM_LAYER_ALE_AUTH_CONNECT_V6,
    &FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V4,
    &FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V6
};
static const wchar_t *LAYER_NAMES[4] = {
    L"outbound IPv4", L"outbound IPv6", L"inbound IPv4", L"inbound IPv6"
};

static void fail(const wchar_t *message, DWORD error) {
    fwprintf(stderr, L"%ls: 0x%08lx\n", message, (unsigned long)error);
}

static int make_filter_key(const wchar_t *path, unsigned layer, GUID *key) {
    HCRYPTPROV provider = 0;
    HCRYPTHASH hash = 0;
    BYTE digest[32];
    DWORD size = sizeof(digest);
    int ok = 0;
    if (!CryptAcquireContextW(&provider, NULL, NULL, PROV_RSA_AES, CRYPT_VERIFYCONTEXT)) goto done;
    if (!CryptCreateHash(provider, CALG_SHA_256, 0, 0, &hash)) goto done;
    if (!CryptHashData(hash, (const BYTE *)path, (DWORD)(wcslen(path) * sizeof(wchar_t)), 0)) goto done;
    if (!CryptHashData(hash, (const BYTE *)&layer, sizeof(layer), 0)) goto done;
    if (!CryptGetHashParam(hash, HP_HASHVAL, digest, &size, 0)) goto done;
    memcpy(key, digest, sizeof(*key));
    ok = 1;
done:
    if (!ok) fail(L"SHA-256 failed", GetLastError());
    if (hash) CryptDestroyHash(hash);
    if (provider) CryptReleaseContext(provider, 0);
    return ok;
}

static int verify_filter(HANDLE engine, const GUID *key, const GUID *layer, const FWP_BYTE_BLOB *app_id) {
    FWPM_FILTER0 *found = NULL;
    DWORD status = FwpmFilterGetByKey0(engine, key, &found);
    if (status) { fail(L"WFP filter lookup failed", status); return 0; }
    int ok = IsEqualGUID(&found->layerKey, layer) &&
        IsEqualGUID(&found->subLayerKey, &VICTORY_SUBLAYER) &&
        found->action.type == FWP_ACTION_BLOCK &&
        (found->flags & FWPM_FILTER_FLAG_PERSISTENT) &&
        found->numFilterConditions == 1 &&
        IsEqualGUID(&found->filterCondition[0].fieldKey, &FWPM_CONDITION_ALE_APP_ID) &&
        found->filterCondition[0].matchType == FWP_MATCH_EQUAL &&
        found->filterCondition[0].conditionValue.type == FWP_BYTE_BLOB_TYPE &&
        found->filterCondition[0].conditionValue.byteBlob &&
        found->filterCondition[0].conditionValue.byteBlob->size == app_id->size &&
        memcmp(found->filterCondition[0].conditionValue.byteBlob->data, app_id->data, app_id->size) == 0;
    if (!ok) fwprintf(stderr, L"WFP filter has unexpected properties\n");
    FwpmFreeMemory0((void **)&found);
    return ok;
}

int wmain(int argc, wchar_t **argv) {
    if (argc != 3 || (wcscmp(argv[1], L"add") && wcscmp(argv[1], L"check") && wcscmp(argv[1], L"remove"))) {
        fwprintf(stderr, L"Usage: victory-offline-wfp.exe add|check|remove ABSOLUTE_EXE_PATH\n");
        return 2;
    }
    wchar_t path[32768];
    DWORD length = GetFullPathNameW(argv[2], 32768, path, NULL);
    if (!length || length >= 32768 || wcscmp(path, argv[2])) {
        fwprintf(stderr, L"Supply a normalized absolute executable path\n");
        return 2;
    }
    for (wchar_t *p = path; *p; ++p) {
        if (*p >= L'A' && *p <= L'Z') *p += L'a' - L'A';
    }
    DWORD attributes = GetFileAttributesW(path);
    size_t path_length = wcslen(path);
    if (attributes == INVALID_FILE_ATTRIBUTES || (attributes & FILE_ATTRIBUTE_DIRECTORY) ||
        path_length < 4 || wcscmp(path + path_length - 4, L".exe")) {
        fwprintf(stderr, L"Executable is missing: %ls\n", path);
        return 2;
    }
    HANDLE engine = NULL;
    DWORD status = FwpmEngineOpen0(NULL, RPC_C_AUTHN_WINNT, NULL, NULL, &engine);
    if (status) { fail(L"WFP engine open failed", status); return 1; }
    FWP_BYTE_BLOB *app_id = NULL;
    status = FwpmGetAppIdFromFileName0(path, &app_id);
    if (status) { fail(L"WFP app ID failed", status); FwpmEngineClose0(engine); return 1; }
    int success = 1;
    if (!wcscmp(argv[1], L"add")) {
        FWPM_SUBLAYER0 sublayer = {0};
        sublayer.subLayerKey = VICTORY_SUBLAYER;
        sublayer.displayData.name = L"VIctory offline executable isolation";
        sublayer.displayData.description = L"Blocks all known VIctory and title executables";
        sublayer.flags = FWPM_SUBLAYER_FLAG_PERSISTENT;
        sublayer.weight = 0xffff;
        status = FwpmSubLayerAdd0(engine, &sublayer, NULL);
        if (status && status != ALREADY_EXISTS) { fail(L"WFP sublayer add failed", status); success = 0; }
    }
    if (success && wcscmp(argv[1], L"remove")) {
        FWPM_SUBLAYER0 *found = NULL;
        status = FwpmSubLayerGetByKey0(engine, &VICTORY_SUBLAYER, &found);
        if (status) { fail(L"WFP sublayer lookup failed", status); success = 0; }
        else {
            if (!(found->flags & FWPM_SUBLAYER_FLAG_PERSISTENT) || found->weight < 0xfffe) {
                fwprintf(stderr, L"WFP sublayer has unexpected properties: flags=0x%04x weight=0x%04x\n",
                    found->flags, found->weight);
                success = 0;
            }
            FwpmFreeMemory0((void **)&found);
        }
    }
    for (unsigned i = 0; success && i < 4; ++i) {
        GUID key;
        if (!make_filter_key(path, i, &key)) { success = 0; break; }
        if (!wcscmp(argv[1], L"add")) {
            FWPM_FILTER_CONDITION0 condition = {0};
            condition.fieldKey = FWPM_CONDITION_ALE_APP_ID;
            condition.matchType = FWP_MATCH_EQUAL;
            condition.conditionValue.type = FWP_BYTE_BLOB_TYPE;
            condition.conditionValue.byteBlob = app_id;
            FWPM_FILTER0 filter = {0};
            filter.filterKey = key;
            filter.displayData.name = L"VIctory offline connection block";
            filter.displayData.description = (wchar_t *)LAYER_NAMES[i];
            filter.flags = FWPM_FILTER_FLAG_PERSISTENT;
            filter.layerKey = *LAYERS[i];
            filter.subLayerKey = VICTORY_SUBLAYER;
            filter.weight.type = FWP_EMPTY;
            filter.numFilterConditions = 1;
            filter.filterCondition = &condition;
            filter.action.type = FWP_ACTION_BLOCK;
            status = FwpmFilterAdd0(engine, &filter, NULL, NULL);
            if (status && status != ALREADY_EXISTS) { fail(L"WFP filter add failed", status); success = 0; break; }
        }
        if (!wcscmp(argv[1], L"remove")) {
            status = FwpmFilterDeleteByKey0(engine, &key);
            if (status && status != FILTER_NOT_FOUND) { fail(L"WFP filter remove failed", status); success = 0; break; }
        } else if (!verify_filter(engine, &key, LAYERS[i], app_id)) { success = 0; break; }
        wprintf(L"%ls %ls: %ls\n", argv[1], LAYER_NAMES[i], path);
    }
    FwpmFreeMemory0((void **)&app_id);
    FwpmEngineClose0(engine);
    return success ? 0 : 1;
}
