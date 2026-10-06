#include "backupsteps.h"

QVariantMap BackupSteps::toDBus() const
{
    return {
        {QStringLiteral("snapshot"), snapshot},
        {QStringLiteral("send"), send},
        {QStringLiteral("boot_archive"), bootArchive},
        {QStringLiteral("index"), index},
        {QStringLiteral("email"), email},
    };
}
