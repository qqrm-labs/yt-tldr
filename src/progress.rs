use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    #[default]
    Accepted,
    Transcribing,
    Summarizing,
    RetryScheduled,
    Delivering,
    Completed,
    Failed,
    DeliveryRejected,
    NeedsReview,
    ConfigurationError,
}

#[derive(Default, Deserialize, Serialize)]
pub struct Progress {
    pub enabled: bool,
    /// At most one creation attempt. An unknown receipt never causes a resend.
    pub creation_attempted: bool,
    pub message_id: Option<i64>,
    pub stage: Stage,
}

impl Stage {
    pub fn text(self, attempts: u8) -> String {
        match self {
            Self::Accepted => "✅ Ссылку получил. Начинаю обработку видео.".into(),
            Self::Transcribing => "🎧 Получаю расшифровку видео.".into(),
            Self::Summarizing => "📝 Расшифровка получена. Готовлю саммари.".into(),
            Self::RetryScheduled => format!(
                "⏳ Временная ошибка или лимит ожидания. Повторю автоматически. Попыток обработки: {attempts} из 3."
            ),
            Self::Delivering => "📨 Саммари готово. Отправляю результат.".into(),
            Self::Completed => "✅ Готово. Результат отправлен.".into(),
            Self::Failed => "⚠️ Не получилось обработать видео. Попробуй отправить ссылку позже.".into(),
            Self::DeliveryRejected => "⚠️ Telegram отклонил отправку результата. Автоматически повторять этот запрос не буду.".into(),
            Self::NeedsReview => "⚠️ Не удалось подтвердить доставку результата. Автоповтор остановлен, чтобы не прислать дубликат; требуется проверка.".into(),
            Self::ConfigurationError => "⚠️ Telegram отклонил запрос из-за настройки доступа. Обработка остановлена, требуется проверка.".into(),
        }
    }
}
