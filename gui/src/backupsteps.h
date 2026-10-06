#pragma once

#include <QVariantMap>

// The operations a GUI backup run performs, as ticked. Sent with BackupRun as
// its steps dictionary (a{sv}); the helper refuses a missing or unknown key,
// a value that is not a boolean, and a run with neither Snapshot nor Send
// (bd DAS-Backup-Manager-c4x). The keys are backup::RUN_STEP_KEYS.
struct BackupSteps {
    bool snapshot = true;
    bool send = true;
    bool bootArchive = true;
    bool index = true;
    bool email = true;

    [[nodiscard]] bool runsBtrbk() const { return snapshot || send; }
    [[nodiscard]] QVariantMap toDBus() const;
};
