//! The window's languages. English is the default, so a test and a log do not depend on Windows' language. The engine's lines, the results
//! file and the log stay English: they are the record. This is the chrome the user clicks and reads.

/// `window.json`'s `language`
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Language {
    #[default]
    En,
    It,
    Ru,
    Zh,
    Ja,
    Es,
}

impl Language {
    pub const CODES: [(&str, Language); 6] = [
        ("en", Language::En),
        ("it", Language::It),
        ("ru", Language::Ru),
        ("zh", Language::Zh),
        ("ja", Language::Ja),
        ("es", Language::Es),
    ];

    /// Each name in its own language, so a user can find it when the rest of the window is the wrong one
    pub const LABELS: [&str; 6] = ["English", "Italiano", "Русский", "中文", "日本語", "Español"];

    pub fn index(self) -> usize {
        Self::CODES.iter().position(|(_, language)| *language == self).unwrap_or(0)
    }

    pub fn at(index: usize) -> Self {
        Self::CODES.get(index).map_or(Language::En, |(_, language)| *language)
    }

    pub fn code(self) -> &'static str {
        Self::CODES.iter().find(|(_, language)| *language == self).map_or("en", |(code, _)| code)
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::CODES.iter().find(|(code, _)| *code == text).map(|(_, language)| *language)
    }
}

/// A line of the window, in `language`
pub fn text(language: Language, phrase: Phrase) -> &'static str {
    row(phrase)[language.index()]
}

/// The `…` menu's items. The menu gives back the label it showed, in whichever language, so a click is matched against every language.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Menu {
    Log,
    Logs,
    Results,
    ResultsFolder,
    Sanitize,
    SanitizeSingle,
    Calibrate,
    Readings,
    Settings,
    About,
}

pub fn menu_label(language: Language, item: Menu) -> &'static str {
    text(language, menu_phrase(item))
}

#[cfg(test)]
pub fn menu_of(label: &str) -> Option<Menu> {
    [
        Menu::Log,
        Menu::Logs,
        Menu::Results,
        Menu::ResultsFolder,
        Menu::Sanitize,
        Menu::SanitizeSingle,
        Menu::Calibrate,
        Menu::Readings,
        Menu::Settings,
        Menu::About,
    ]
    .into_iter()
    .find(|item| row(menu_phrase(*item)).contains(&label))
}

pub fn tab(language: Language, index: usize) -> &'static str {
    text(language, tab_phrase(index))
}

pub fn tab_index(label: &str) -> Option<usize> {
    (0..7).find(|index| row(tab_phrase(*index)).contains(&label))
}

pub fn status(language: Language, index: usize) -> &'static str {
    text(language, status_phrase(index))
}

/// About's version, without the build profile or the commit
pub fn version_line(language: Language, version: &str) -> String {
    match language {
        Language::En => format!("Version {version}"),
        Language::It => format!("Versione {version}"),
        Language::Ru => format!("Версия {version}"),
        Language::Zh => format!("版本 {version}"),
        Language::Ja => format!("バージョン {version}"),
        Language::Es => format!("Versión {version}"),
    }
}

/// About's author line. `name` is the repository owner's, as the crate records it.
pub fn by_line(language: Language, name: &str) -> String {
    match language {
        Language::En => format!("by {name}"),
        Language::It => format!("di {name}"),
        Language::Ru => format!("автор {name}"),
        Language::Zh => format!("作者 {name}"),
        Language::Ja => format!("作者 {name}"),
        Language::Es => format!("de {name}"),
    }
}

/// This build is the latest published release, or newer than it
pub fn up_to_date(language: Language, version: &str) -> String {
    match language {
        Language::En => format!("This is the latest release ({version})"),
        Language::It => format!("Questa è l'ultima release ({version})"),
        Language::Ru => format!("Это последний выпуск ({version})"),
        Language::Zh => format!("这是最新版本（{version}）"),
        Language::Ja => format!("これが最新のリリースです（{version}）"),
        Language::Es => format!("Esta es la última versión ({version})"),
    }
}

pub fn update_available(language: Language, version: &str) -> String {
    match language {
        Language::En => format!("{version} is available"),
        Language::It => format!("{version} è disponibile"),
        Language::Ru => format!("Доступна {version}"),
        Language::Zh => format!("有新版本 {version}"),
        Language::Ja => format!("{version} が公開されています"),
        Language::Es => format!("{version} está disponible"),
    }
}

pub fn ahead_of_release(language: Language, version: &str) -> String {
    match language {
        Language::En => format!("This build is newer than the latest release ({version})"),
        Language::It => format!("Questa build è più nuova dell'ultima release ({version})"),
        Language::Ru => format!("Эта сборка новее последнего выпуска ({version})"),
        Language::Zh => format!("此构建新于最新版本（{version}）"),
        Language::Ja => format!("このビルドは最新リリース（{version}）より新しいです"),
        Language::Es => format!("Esta compilación es más nueva que la última versión ({version})"),
    }
}

pub fn update_failed(language: Language, why: &str) -> String {
    match language {
        Language::En => format!("Couldn't check for updates: {why}"),
        Language::It => format!("Impossibile cercare aggiornamenti: {why}"),
        Language::Ru => format!("Не удалось проверить обновления: {why}"),
        Language::Zh => format!("无法检查更新：{why}"),
        Language::Ja => format!("更新を確認できませんでした：{why}"),
        Language::Es => format!("No se pudo buscar actualizaciones: {why}"),
    }
}

/// `{free}` GiB free of `{total}`, already rounded
pub fn ram(language: Language, free: f64, total: f64) -> String {
    match language {
        Language::En => format!("{free:.1} GiB free of {total:.1}"),
        Language::It => format!("{free:.1} GiB liberi su {total:.1}"),
        Language::Ru => format!("свободно {free:.1} ГиБ из {total:.1}"),
        Language::Zh => format!("空闲 {free:.1} GiB，共 {total:.1}"),
        Language::Ja => format!("{total:.1} 中 {free:.1} GiB 空き"),
        Language::Es => format!("{free:.1} GiB libres de {total:.1}"),
    }
}

/// What the window says. Order is the language order: English, Italian, Russian, Chinese, Japanese, Spanish.
#[derive(Clone, Copy, Debug)]
pub enum Phrase {
    Start,
    Stop,
    Stopping,
    Finishing,
    RunAgain,
    LoadConfig,
    LoadConfigTitle,
    MenuButton,
    MenuFiles,
    MenuTools,
    MenuApp,
    Choose,
    ThisRun,
    ChooseBoth,
    Swap,
    CompareBaseline,
    CompareOther,
    CompareBothTitle,
    Baseline,
    Compared,
    Plan,
    Memory,
    Cycles,
    Errors,
    OpenLog,
    OpenLogs,
    OpenResults,
    OpenResultsFolder,
    Sanitize,
    SanitizeSingle,
    Calibrate,
    Readings,
    Settings,
    About,
    Close,
    ReplaceTitle,
    Replace,
    Keep,
    UnfinishedTitle,
    UnfinishedBody,
    OpenTheResults,
    Caution,
    NoRam,
    NoTimings,
    NoErrors,
    NoSteps,
    LogHint,
    Parameters,
    ParametersTip,
    Theme,
    LanguageLabel,
    Kept,
    StartOnOpen,
    Serials,
    Notify,
    Updates,
    CheckUpdates,
    CheckingUpdates,
    OpenRelease,
    AsWindows,
    Light,
    Dark,
    Ready,
    Running,
    StoppingBadge,
    Passed,
    Failed,
    Interrupted,
    NotRun,
    Calibrating,
    TabRun,
    TabLog,
    TabPlan,
    TabSystem,
    TabThreads,
    TabMemory,
    TabCompare,
    SystemInfo,
    RuntimeTip,
}

fn menu_phrase(item: Menu) -> Phrase {
    match item {
        Menu::Log => Phrase::OpenLog,
        Menu::Logs => Phrase::OpenLogs,
        Menu::Results => Phrase::OpenResults,
        Menu::ResultsFolder => Phrase::OpenResultsFolder,
        Menu::Sanitize => Phrase::Sanitize,
        Menu::SanitizeSingle => Phrase::SanitizeSingle,
        Menu::Calibrate => Phrase::Calibrate,
        Menu::Readings => Phrase::Readings,
        Menu::Settings => Phrase::Settings,
        Menu::About => Phrase::About,
    }
}

fn tab_phrase(index: usize) -> Phrase {
    [Phrase::TabRun, Phrase::TabLog, Phrase::TabPlan, Phrase::TabSystem, Phrase::TabThreads, Phrase::TabMemory, Phrase::TabCompare]
        .get(index)
        .copied()
        .unwrap_or(Phrase::TabRun)
}

fn status_phrase(index: usize) -> Phrase {
    [
        Phrase::Ready,
        Phrase::Running,
        Phrase::StoppingBadge,
        Phrase::Passed,
        Phrase::Failed,
        Phrase::Interrupted,
        Phrase::NotRun,
        Phrase::Calibrating,
    ]
    .get(index)
    .copied()
    .unwrap_or(Phrase::Ready)
}

fn row(phrase: Phrase) -> [&'static str; 6] {
    match phrase {
        Phrase::Start => ["Start", "Avvia", "Старт", "开始", "開始", "Iniciar"],
        Phrase::Stop => ["Stop", "Ferma", "Стоп", "停止", "停止", "Detener"],
        Phrase::Stopping => ["Stopping…", "Arresto…", "Остановка…", "正在停止…", "停止しています…", "Deteniendo…"],
        Phrase::Finishing => ["Finishing…", "Chiusura…", "Завершение…", "正在结束…", "終了しています…", "Terminando…"],
        Phrase::RunAgain => ["Run again", "Esegui di nuovo", "Ещё раз", "再次运行", "再実行", "Ejecutar de nuevo"],
        Phrase::LoadConfig => ["Load config…", "Carica config…", "Загрузить конфиг…", "加载配置…", "設定を読み込む…", "Cargar config…"],
        Phrase::MenuButton => ["Menu", "Menu", "Меню", "菜单", "メニュー", "Menú"],
        Phrase::MenuFiles => ["Files", "File", "Файлы", "文件", "ファイル", "Archivos"],
        Phrase::MenuTools => ["Tools", "Strumenti", "Инструменты", "工具", "ツール", "Herramientas"],
        Phrase::MenuApp => ["App", "App", "Приложение", "应用", "アプリ", "Aplicación"],
        Phrase::LoadConfigTitle => [
            "Load a config: TMR's (.json) or TM5's (.cfg)",
            "Carica una config: di TMR (.json) o di TM5 (.cfg)",
            "Загрузить конфиг: TMR (.json) или TM5 (.cfg)",
            "加载配置：TMR（.json）或 TM5（.cfg）",
            "設定を読み込む：TMR（.json）または TM5（.cfg）",
            "Cargar una config: de TMR (.json) o de TM5 (.cfg)",
        ],
        Phrase::Choose => ["Choose…", "Scegli…", "Выбрать…", "选择…", "選択…", "Elegir…"],
        Phrase::ThisRun => ["This run", "Questo run", "Этот прогон", "本次运行", "この実行", "Esta ejecución"],
        Phrase::ChooseBoth => ["Choose both…", "Scegli entrambi…", "Выбрать оба…", "选择两个…", "両方を選択…", "Elegir ambos…"],
        Phrase::Swap => ["Swap", "Scambia", "Поменять", "交换", "入れ替え", "Intercambiar"],
        Phrase::CompareBaseline => [
            "Compare: the baseline's results file",
            "Confronta: il file dei risultati di riferimento",
            "Сравнение: файл результатов базового прогона",
            "比较：基准的结果文件",
            "比較：基準の結果ファイル",
            "Comparar: el archivo de resultados de la referencia",
        ],
        Phrase::CompareOther => [
            "Compare: the results file to compare with the baseline",
            "Confronta: il file dei risultati da confrontare col riferimento",
            "Сравнение: файл результатов для сравнения с базовым",
            "比较：要与基准比较的结果文件",
            "比較：基準と比較する結果ファイル",
            "Comparar: el archivo de resultados a comparar con la referencia",
        ],
        Phrase::CompareBothTitle => [
            "Compare two results files: choose both, the second with Ctrl",
            "Confronta due file dei risultati: scegli entrambi, il secondo con Ctrl",
            "Сравнить два файла результатов: выберите оба, второй с Ctrl",
            "比较两个结果文件：选择两个，第二个用 Ctrl",
            "結果ファイルを二つ比較：両方を選択、二つ目は Ctrl",
            "Comparar dos archivos de resultados: elige ambos, el segundo con Ctrl",
        ],
        Phrase::Baseline => ["Baseline", "Riferimento", "Базовый", "基准", "基準", "Referencia"],
        Phrase::Compared => ["Compared", "Confronto", "Сравниваемый", "对比", "比較対象", "Comparada"],
        Phrase::Plan => ["Plan", "Piano", "План", "计划", "プラン", "Plan"],
        Phrase::Memory => ["Memory", "Memoria", "Память", "内存", "メモリ", "Memoria"],
        Phrase::Cycles => ["Cycles", "Cicli", "Циклы", "周期", "サイクル", "Ciclos"],
        Phrase::Errors => ["Errors", "Errori", "Ошибки", "错误", "エラー", "Errores"],
        Phrase::OpenLog => ["Open the log", "Apri il log", "Открыть журнал", "打开日志", "ログを開く", "Abrir el registro"],
        Phrase::OpenLogs => ["Logs folder", "Cartella log", "Папка журналов", "日志文件夹", "ログフォルダー", "Carpeta de registros"],
        Phrase::OpenResults => {
            ["Results file", "File dei risultati", "Файл результатов", "结果文件", "結果ファイル", "Archivo de resultados"]
        }
        Phrase::OpenResultsFolder => {
            ["Results folder", "Cartella risultati", "Папка результатов", "结果文件夹", "結果フォルダー", "Carpeta de resultados"]
        }
        Phrase::Calibrate => ["Calibrate", "Calibra", "Калибровка", "校准", "較正", "Calibrar"],
        Phrase::Readings => {
            ["Timings tools", "Strumenti dei timing", "Утилиты таймингов", "时序工具", "タイミングツール", "Herramientas de timings"]
        }
        Phrase::Settings => ["Settings", "Impostazioni", "Параметры", "设置", "設定", "Ajustes"],
        Phrase::About => ["About TMR", "Informazioni su TMR", "О TMR", "关于 TMR", "TMR について", "Acerca de TMR"],
        Phrase::Close => ["Close", "Chiudi", "Закрыть", "关闭", "閉じる", "Cerrar"],
        Phrase::ReplaceTitle => [
            "Replace the stored calibration?",
            "Sostituire la calibrazione salvata?",
            "Заменить сохранённую калибровку?",
            "替换已保存的校准？",
            "保存済みの較正を置き換えますか？",
            "¿Reemplazar la calibración guardada?",
        ],
        Phrase::Replace => ["Replace it", "Sostituiscila", "Заменить", "替换", "置き換える", "Reemplazarla"],
        Phrase::Keep => [
            "Keep the stored one",
            "Tieni quella salvata",
            "Оставить сохранённую",
            "保留已保存的",
            "保存済みを残す",
            "Conservar la guardada",
        ],
        Phrase::UnfinishedTitle => [
            "The last run did not finish",
            "L'ultimo run non è finito",
            "Последний прогон не завершился",
            "上次运行未完成",
            "前回の実行は完了していません",
            "La última ejecución no terminó",
        ],
        Phrase::UnfinishedBody => [
            "The window saved this while the run was going. A crash or a reset keeps the last note, not a picture of the moment it died.",
            "La finestra ha salvato questo mentre il run era in corso. Un crash o un reset tengono l'ultima nota, non una foto dell'istante della morte.",
            "Окно сохранило это во время прогона. Сбой или перезагрузка оставляют последнюю запись, а не снимок момента смерти.",
            "窗口在运行期间保存了这些。崩溃或重启留下的是最后一条记录，不是死机那一刻的截图。",
            "実行中にウィンドウがこれを保存しました。クラッシュや再起動で残るのは最後の記録で、停止した瞬間の画面ではありません。",
            "La ventana guardó esto mientras la ejecución seguía. Un cuelgue o un reinicio dejan la última nota, no una foto del instante.",
        ],
        Phrase::OpenTheResults => {
            ["Open the results", "Apri i risultati", "Открыть результаты", "打开结果", "結果を開く", "Abrir los resultados"]
        }
        Phrase::Caution => [
            "An unstable overclock can freeze this PC. Save your work first.",
            "Un overclock instabile può bloccare questo PC. Salva il lavoro prima.",
            "Нестабильный разгон может заморозить этот ПК. Сначала сохраните работу.",
            "不稳定的超频可能使这台电脑死机。请先保存工作。",
            "不安定なオーバークロックはこの PC をフリーズさせることがあります。先に作業を保存してください。",
            "Un overclock inestable puede congelar este PC. Guarda el trabajo antes.",
        ],
        Phrase::NoRam => [
            "Windows didn't say how much RAM is free",
            "Windows non ha detto quanta RAM è libera",
            "Windows не сообщил, сколько памяти свободно",
            "Windows 没有报告剩余内存",
            "Windows は空きメモリを報告しませんでした",
            "Windows no dijo cuánta RAM está libre",
        ],
        Phrase::NoTimings => [
            "no timings yet: Timings tools, in the menu",
            "ancora nessun timing: Strumenti dei timing, nel menu",
            "таймингов пока нет: Утилиты таймингов, в меню",
            "还没有时序：菜单中的时序工具",
            "タイミングはまだありません：メニューのタイミングツール",
            "aún no hay timings: Herramientas de timings, en el menú",
        ],
        Phrase::NoErrors => [
            "No errors so far", "Nessun errore finora", "Ошибок пока нет", "目前没有错误", "エラーはまだありません",
            "Ningún error hasta ahora",
        ],
        Phrase::NoSteps => [
            "No steps finished yet",
            "Nessun passo finito",
            "Шаги ещё не закончились",
            "还没有完成的步骤",
            "完了したステップはまだありません",
            "Ningún paso terminado",
        ],
        Phrase::LogHint => [
            "The log's lines: from Start",
            "Le righe del log: da Avvia",
            "Строки журнала: со Старта",
            "日志行：从开始",
            "ログの行：開始から",
            "Líneas del registro: desde Iniciar",
        ],
        Phrase::Parameters => [
            "Parameters, as on the command line: memory=50% cycles=3 (none: the defaults)",
            "Parametri, come sulla riga di comando: memory=50% cycles=3 (vuoto: i predefiniti)",
            "Параметры, как в командной строке: memory=50% cycles=3 (пусто: по умолчанию)",
            "参数，与命令行相同：memory=50% cycles=3（空：默认值）",
            "コマンドラインと同じパラメータ：memory=50% cycles=3（空：既定値）",
            "Parámetros, como en la línea de comandos: memory=50% cycles=3 (vacío: los predeterminados)",
        ],
        Phrase::ParametersTip => [
            "What Start runs, in tmr.exe's key=value syntax (tmr.exe --help lists them). Quote one with a space in it.",
            "Ciò che Avvia esegue, nella sintassi chiave=valore di tmr.exe (tmr.exe --help le elenca). Virgolette se c'è uno spazio.",
            "Что запускает Старт, в синтаксисе ключ=значение tmr.exe (tmr.exe --help перечисляет их). Кавычки, если есть пробел.",
            "开始所运行的内容，tmr.exe 的 key=value 语法（tmr.exe --help 列出全部）。含空格时请加引号。",
            "開始が実行する内容。tmr.exe の key=value 構文（tmr.exe --help に一覧）。空白を含む場合は引用符で。",
            "Lo que ejecuta Iniciar, en la sintaxis clave=valor de tmr.exe (tmr.exe --help las lista). Comillas si hay un espacio.",
        ],
        Phrase::Theme => ["Theme", "Tema", "Тема", "主题", "テーマ", "Tema"],
        Phrase::LanguageLabel => ["Language", "Lingua", "Язык", "语言", "言語", "Idioma"],
        Phrase::Kept => [
            "Kept in window.json, in TMR's settings folder, as each changes",
            "Tenuto in window.json, nella cartella delle impostazioni di TMR, a ogni modifica",
            "Хранится в window.json, в папке настроек TMR, при каждом изменении",
            "保存在 TMR 设置文件夹的 window.json 中，每次更改都会写入",
            "TMR の設定フォルダーの window.json に、変更のたびに保存されます",
            "Se guarda en window.json, en la carpeta de configuración de TMR, a cada cambio",
        ],
        Phrase::StartOnOpen => [
            "Start the run as the window opens, with the parameters given or the last run's",
            "Avvia il run all'apertura, con i parametri dati o quelli dell'ultimo run",
            "Запускать прогон при открытии окна, с данными параметрами или с прошлого раза",
            "窗口打开时开始运行，使用给定参数或上次的参数",
            "ウィンドウを開いたときに実行を開始。指定したパラメータ、または前回のもの",
            "Iniciar la ejecución al abrir la ventana, con los parámetros dados o los de la última",
        ],
        Phrase::Sanitize => [
            "Sanitize results and log…",
            "Oscura dati nei risultati e nel log…",
            "Обезличить результаты и журнал…",
            "清理结果和日志…",
            "結果とログを匿名化…",
            "Anonimizar resultados y registro…",
        ],
        Phrase::SanitizeSingle => [
            "Sanitize one file…",
            "Oscura dati in un file…",
            "Обезличить один файл…",
            "清理单个文件…",
            "ファイルを匿名化…",
            "Anonimizar un archivo…",
        ],
        Phrase::Serials => [
            "Keep serial numbers, the DIMMs' and the system's UUID, in the run's output, log and results file",
            "Tieni i numeri di serie, dei DIMM e l'UUID del sistema, nell'output, nel log e nei risultati",
            "Хранить серийные номера DIMM и UUID системы в выводе, журнале и результатах",
            "在输出、日志和结果中保留 DIMM 序列号和系统 UUID",
            "出力、ログ、結果に DIMM のシリアルとシステムの UUID を残す",
            "Guardar los números de serie de los DIMM y el UUID del sistema en la salida, el registro y los resultados",
        ],
        Phrase::Notify => [
            "Sound at the first error, and when the run ends",
            "Suono al primo errore e alla fine del run",
            "Звук при первой ошибке и в конце прогона",
            "第一次出错时和运行结束时播放声音",
            "最初のエラーと実行終了時に音を鳴らす",
            "Sonido en el primer error y al terminar la ejecución",
        ],
        Phrase::Updates => ["Updates", "Aggiornamenti", "Обновления", "更新", "更新", "Actualizaciones"],
        Phrase::CheckUpdates => {
            ["Check for updates", "Cerca aggiornamenti", "Проверить обновления", "检查更新", "更新を確認", "Buscar actualizaciones"]
        }
        Phrase::CheckingUpdates => ["Checking…", "Controllo…", "Проверка…", "正在检查…", "確認しています…", "Comprobando…"],
        Phrase::OpenRelease => {
            ["Open the release", "Apri la release", "Открыть выпуск", "打开该版本", "リリースを開く", "Abrir la versión"]
        }
        Phrase::AsWindows => ["As Windows", "Come Windows", "Как в Windows", "跟随 Windows", "Windows に合わせる", "Como Windows"],
        Phrase::Light => ["Light", "Chiaro", "Светлая", "浅色", "ライト", "Claro"],
        Phrase::Dark => ["Dark", "Scuro", "Тёмная", "深色", "ダーク", "Oscuro"],
        Phrase::Ready => ["Ready", "Pronto", "Готов", "就绪", "準備完了", "Listo"],
        Phrase::Running => ["Running", "In esecuzione", "Выполняется", "运行中", "実行中", "En ejecución"],
        Phrase::StoppingBadge => ["Stopping", "Arresto", "Остановка", "正在停止", "停止中", "Deteniendo"],
        Phrase::Passed => ["Passed", "Superato", "Пройден", "通过", "合格", "Correcto"],
        Phrase::Failed => ["Failed", "Fallito", "Провал", "失败", "失敗", "Fallido"],
        Phrase::Interrupted => ["Interrupted", "Interrotto", "Прерван", "已中断", "中断", "Interrumpido"],
        Phrase::NotRun => ["Not run", "Non eseguito", "Не запущен", "未运行", "未実行", "No ejecutado"],
        Phrase::Calibrating => ["Calibrating", "Calibrazione", "Калибровка", "校准中", "較正中", "Calibrando"],
        Phrase::TabRun => ["Run", "Esecuzione", "Прогон", "运行", "実行", "Ejecución"],
        Phrase::TabLog => ["Log", "Log", "Журнал", "日志", "ログ", "Registro"],
        Phrase::TabPlan => ["Plan", "Piano", "План", "计划", "プラン", "Plan"],
        Phrase::TabSystem => ["System", "Sistema", "Система", "系统", "システム", "Sistema"],
        Phrase::TabThreads => ["Threads", "Thread", "Потоки", "线程", "スレッド", "Hilos"],
        Phrase::TabMemory => ["Memory", "Memoria", "Память", "内存", "メモリ", "Memoria"],
        Phrase::TabCompare => ["Compare", "Confronta", "Сравнение", "比较", "比較", "Comparar"],
        Phrase::SystemInfo => [
            "System info: at Start",
            "Info di sistema: da Avvia",
            "Сведения о системе: со Старта",
            "系统信息：从开始",
            "システム情報：開始から",
            "Info del sistema: desde Iniciar",
        ],
        Phrase::RuntimeTip => [
            "The suite's runtime, from its first cycle, as the console's ticker counts it",
            "Il tempo della suite, dal primo ciclo, come lo conta il ticker della console",
            "Время набора, с первого цикла, как его считает тикер консоли",
            "套件运行时间，从第一个周期起，与控制台计时相同",
            "スイートの実行時間。最初のサイクルから、コンソールのティッカーと同じ",
            "El tiempo de la suite, desde el primer ciclo, como lo cuenta el ticker de la consola",
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_has_a_line_and_a_menu_click_finds_it() {
        for phrase in [Phrase::Start, Phrase::Settings, Phrase::UnfinishedTitle, Phrase::Caution, Phrase::TabRun, Phrase::Calibrating] {
            for language in Language::CODES.iter().map(|(_, language)| *language) {
                assert!(!text(language, phrase).is_empty(), "{phrase:?} {language:?}");
            }
        }
        assert_eq!(menu_of("Settings"), Some(Menu::Settings));
        assert_eq!(menu_of("Impostazioni"), Some(Menu::Settings));
        assert_eq!(menu_of("设置"), Some(Menu::Settings));
        assert_eq!(menu_of("設定"), Some(Menu::Settings));
        assert_eq!(tab_index("Confronta"), Some(6));
        assert_eq!(tab_index("Run"), Some(0));
        assert!(menu_of("not a menu").is_none());
    }

    #[test]
    fn every_menu_and_tab_label_maps_back_to_the_same_item() {
        let items = [
            Menu::Log,
            Menu::Logs,
            Menu::Results,
            Menu::ResultsFolder,
            Menu::Sanitize,
            Menu::SanitizeSingle,
            Menu::Calibrate,
            Menu::Readings,
            Menu::Settings,
            Menu::About,
        ];
        let mut seen = std::collections::HashMap::new();
        for item in items {
            for language in Language::CODES.iter().map(|(_, language)| *language) {
                let label = menu_label(language, item);
                assert!(!label.is_empty(), "{item:?} {language:?}");
                if let Some(other) = seen.insert(label, item) {
                    assert_eq!(other, item, "{label:?} is both {other:?} and {item:?}");
                }
                assert_eq!(menu_of(label), Some(item), "{label:?}");
            }
        }
        let mut tabs = std::collections::HashMap::new();
        for index in 0..7 {
            for language in Language::CODES.iter().map(|(_, language)| *language) {
                let label = tab(language, index);
                assert!(!label.is_empty(), "tab {index} {language:?}");
                if let Some(other) = tabs.insert(label, index) {
                    assert_eq!(other, index, "{label:?} is both tab {other} and tab {index}");
                }
                assert_eq!(tab_index(label), Some(index), "{label:?}");
            }
        }
        assert!(version_line(Language::Ja, "0.1.2").contains("0.1.2"));
        assert!(by_line(Language::It, "Jarrod Petz").contains("Jarrod Petz"));
    }
}
