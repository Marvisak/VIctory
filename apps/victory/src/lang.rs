//! The menu's language: Portuguese where Windows' display language is,
//! English otherwise; `VICTORY_LANG=en` or `pt` chooses.
//!
//! The menu builds each screen in English and [`localize`] translates the
//! finished text by whole phrases (`PT`), so its logic and tests stay in one
//! language. Only the menu's own text goes through it: a run's log and the
//! report copied for GitHub stay as they are.

use std::sync::OnceLock;

/// Whether the menu speaks Portuguese (decided once).
pub fn portuguese() -> bool {
    static PT_UI: OnceLock<bool> = OnceLock::new();
    *PT_UI.get_or_init(|| {
        if cfg!(test) {
            return false;
        }
        match std::env::var("VICTORY_LANG").as_deref().map(str::trim) {
            Ok(v) if v.eq_ignore_ascii_case("pt") || v.to_ascii_lowercase().starts_with("pt-") => {
                true
            }
            Ok(v) if v.eq_ignore_ascii_case("en") || v.to_ascii_lowercase().starts_with("en-") => {
                false
            }
            _ => windows_is_portuguese(),
        }
    })
}

#[cfg(windows)]
fn windows_is_portuguese() -> bool {
    // SAFETY: a plain query with no arguments.
    let id = unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() };
    // The primary language: LANG_PORTUGUESE.
    id & 0x3FF == 0x16
}

#[cfg(not(windows))]
fn windows_is_portuguese() -> bool {
    false
}

thread_local! {
    /// Set while [`in_english`] runs: the text built then stays English.
    static ENGLISH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `f` with [`localize`] leaving text in English (the report copied
/// for GitHub).
pub fn in_english<T>(f: impl FnOnce() -> T) -> T {
    let was = ENGLISH.replace(true);
    let out = f();
    ENGLISH.set(was);
    out
}

/// `text` (a screen the menu built) in the menu's language.
pub fn localize(text: &str) -> String {
    if portuguese() && !ENGLISH.get() {
        to_portuguese(text)
    } else {
        text.to_owned()
    }
}

/// `text` with every phrase of [`PT`] translated, longest first so a phrase
/// is never cut by a shorter one inside it.
pub fn to_portuguese(text: &str) -> String {
    let mut pairs: Vec<&(&str, &str)> = PT.iter().collect();
    pairs.sort_by_key(|(en, _)| std::cmp::Reverse(en.len()));
    let mut out = text.to_owned();
    for (en, pt) in pairs {
        if out.contains(en) {
            out = out.replace(en, pt);
        }
    }
    out
}

/// English phrase as the menu (and the doctor) write it, then Portuguese.
pub const PT: &[(&str, &str)] = &[
    // The menu.
    (
        "your own copy of the title, on this PC",
        "a sua cópia do jogo, neste PC",
    ),
    ("Run a game", "Rodar um jogo"),
    ("Run again", "Rodar de novo"),
    ("(nothing run yet)", "(nada rodado ainda)"),
    ("Check this PC", "Checar este PC"),
    ("Open the data folder", "Abrir a pasta de dados"),
    ("Set the game folder", "Definir a pasta do jogo"),
    ("(not set)", "(não definida)"),
    ("Keyboard controls", "Controles do teclado"),
    ("(M switches to ", "(M troca para "),
    ("  Quit\n", "  Sair\n"),
    ("Choose:", "Escolha:"),
    (
        "Type 1 to 4, S, K or M and press Enter (Q to quit).",
        "Digite 1 a 4, S, K ou M e aperte Enter (Q para sair).",
    ),
    (
        "Press Enter to go back to the menu.",
        "Aperte Enter para voltar ao menu.",
    ),
    // Running a game.
    (
        "Drag the game's .exe onto this window, or type its path, then press Enter.",
        "Arraste o .exe do jogo para esta janela, ou digite o caminho, e aperte Enter.",
    ),
    ("No file there.", "Não há arquivo aí."),
    ("The run ended.", "A execução terminou."),
    (
        "The run stopped: exit code ",
        "A execução parou: código de saída ",
    ),
    ("The run was stopped.", "A execução foi interrompida."),
    ("The run could not start: ", "A execução não começou: "),
    (
        "Nothing run yet: choose 1 first.",
        "Nada rodado ainda: escolha 1 primeiro.",
    ),
    ("What happened", "O que aconteceu"),
    // The check.
    ("Checking this PC...", "Checando este PC..."),
    ("\n  This PC\n", "\n  Este PC\n"),
    ("\n  Paths\n", "\n  Pastas\n"),
    ("Windows version unknown", "Versão do Windows desconhecida"),
    (
        "Its built-in D3D12 stops at shader model 6.5. A D3D12\\ folder is beside victory.exe; this build must also be made to load it (#175).",
        "O D3D12 que vem com ele para no shader model 6.5. Há uma pasta D3D12\\ ao lado do victory.exe; esta versão também precisa carregá-la (#175).",
    ),
    (
        "Its built-in D3D12 stops at shader model 6.5. VIctory needs 6.6 from the D3D12 Agility SDK runtime (#84, #175).",
        "O D3D12 que vem com ele para no shader model 6.5. O VIctory precisa do 6.6, do runtime D3D12 Agility SDK (#84, #175).",
    ),
    ("CPU has no AVX2", "CPU sem AVX2"),
    ("CPU has AVX2", "CPU com AVX2"),
    ("The title needs it.", "O jogo precisa disso."),
    (
        "GPU: no shader model 6.6 device",
        "GPU: sem dispositivo com shader model 6.6",
    ),
    (
        "Translated shaders need it. On Windows 10, see above.",
        "Os shaders traduzidos precisam disso. No Windows 10, veja acima.",
    ),
    ("GPU: not checked", "GPU: não verificada"),
    ("GPU: no mesh shaders", "GPU: sem mesh shaders"),
    (
        "Menus can run; the world's mesh draws are refused (#3).",
        "Os menus rodam; os desenhos de mesh do mundo são recusados (#3).",
    ),
    ("GPU: waves up to ", "GPU: waves de até "),
    ("GPU: waves of ", "GPU: waves de "),
    (
        "The title's programs are written for 64; some run in halves, some are refused (#578, #778).",
        "Os programas do jogo são escritos para 64; alguns rodam em metades, outros são recusados (#578, #778).",
    ),
    ("Game folder: not set", "Pasta do jogo: não definida"),
    ("Game folder: ", "Pasta do jogo: "),
    (" (not there)", " (não existe)"),
    (
        "Fix game_dir in victory.toml. Running from this menu does not need it.",
        "Corrija game_dir no victory.toml. Rodar por este menu não precisa disso.",
    ),
    (
        "S in the menu sets it (game_dir in victory.toml); 1 asks for the .exe.",
        "S no menu define a pasta (game_dir no victory.toml); 1 pede o .exe.",
    ),
    ("Data folder: ", "Pasta de dados: "),
    ("Cache folder: ", "Pasta de cache: "),
    (
        "Enter: back to the menu.  C, then Enter: copy this for a GitHub report.",
        "Enter: voltar ao menu.  C e Enter: copiar isto para um relato no GitHub.",
    ),
    (
        "Copied. Paste it into the report form on GitHub.",
        "Copiado. Cole no formulário de relato do GitHub.",
    ),
    (
        "Could not reach the clipboard.",
        "Não consegui acessar a área de transferência.",
    ),
    // Folders.
    ("Opened ", "Aberta: "),
    ("Could not open ", "Não consegui abrir "),
    (
        "Drag the game's folder (or its .exe) onto this window, or type its path, then press Enter.",
        "Arraste a pasta do jogo (ou o .exe dela) para esta janela, ou digite o caminho, e aperte Enter.",
    ),
    ("No folder there.", "Não há pasta aí."),
    ("Game folder set in ", "Pasta do jogo definida em "),
    (
        " VICTORY_GAME_DIR is set, and it wins over the file.",
        " VICTORY_GAME_DIR está definida e vale mais que o arquivo.",
    ),
    ("Could not write ", "Não consegui gravar "),
    // The keys.
    (
        "left / right stick click",
        "clique do analógico esquerdo / direito",
    ),
    ("left / right stick", "analógico esquerdo / direito"),
    ("left / right shoulder", "botão de ombro esquerdo / direito"),
    ("left / right trigger", "gatilho esquerdo / direito"),
    ("arrows", "setas"),
    ("D-pad", "direcional"),
    (
        "With no XInput pad connected, the keyboard is a pad while the game's window has focus.",
        "Sem controle XInput conectado, o teclado vira um controle enquanto a janela do jogo está em foco.",
    ),
    // The doctor's findings.
    (
        "Windows 10's built-in D3D12 is too old",
        "O D3D12 que vem no Windows 10 é antigo demais",
    ),
    (
        "VIctory needs shader model 6.6 and descriptor heaps that shaders index directly. Windows 10's built-in D3D12 runtime has neither, so every pipeline's root signature is refused and nothing draws. The D3D12 Agility SDK runtime beside victory.exe provides both.",
        "O VIctory precisa do shader model 6.6 e de heaps de descritores indexados direto pelos shaders. O D3D12 que vem no Windows 10 não tem nenhum dos dois, então a root signature de todo pipeline é recusada e nada é desenhado. O runtime D3D12 Agility SDK ao lado do victory.exe traz os dois.",
    ),
    (
        "The title stopped itself: its instance pool is full",
        "O jogo parou sozinho: a lista de instâncias dele encheu",
    ),
    (
        "The title asserts when its INSTANCE_LIST pool (45,000 entries) fills. In the reports this follows draws being dropped because some programs don't translate yet; fixing the culling shaders refused as 'a structured buffer of unknown stride' let runs go on.",
        "O jogo se interrompe quando a lista INSTANCE_LIST (45.000 entradas) enche. Nos relatos, isso vem de desenhos descartados porque alguns programas ainda não traduzem; corrigir os shaders de culling recusados como 'a structured buffer of unknown stride' fez as execuções continuarem.",
    ),
    (
        "A known crash at Game Init",
        "Um crash conhecido no Game Init",
    ),
    (
        "Reported on Windows 10 with main, where it went away with the Agility runtime. It has also been seen with that runtime present, so it can have other causes.",
        "Relatado no Windows 10 com o main, onde sumiu com o runtime Agility. Também já apareceu com esse runtime presente, então pode ter outras causas.",
    ),
    (
        "This GPU has no mesh shaders",
        "Esta GPU não tem mesh shaders",
    ),
    (
        "The world's mesh pipelines are refused on GPUs without mesh shaders (for example GTX 10 series, RX 400 to 5000). A compute fallback that draws them is proposed.",
        "Os pipelines de mesh do mundo são recusados em GPUs sem mesh shaders (por exemplo, série GTX 10, RX 400 a 5000). Um fallback em compute que os desenha está proposto.",
    ),
    (
        "The GPU hung (device removed)",
        "A GPU travou (dispositivo removido)",
    ),
    (
        "Most such reports are on NVIDIA, whose waves are 32 lanes while the title's mesh programs are written for 64. Fixes are in progress; on other GPUs, a report with the DRED output helps.",
        "A maioria desses relatos é em NVIDIA, cujas waves têm 32 lanes enquanto os programas de mesh do jogo são escritos para 64. Há correções em andamento; em outras GPUs, um relato com a saída do DRED ajuda.",
    ),
    (
        "Some programs need 64-lane waves",
        "Alguns programas precisam de waves de 64 lanes",
    ),
    (
        "This GPU runs 32-lane waves, and compute programs translated for 64 are refused until running them in 32-lane halves is supported.",
        "Esta GPU roda waves de 32 lanes, e os programas de compute traduzidos para 64 são recusados até que rodá-los em metades de 32 seja suportado.",
    ),
    ("The GPU ran out of memory", "A memória da GPU acabou"),
    (
        "The world needs more video memory than many 8 GB cards have. A lower texture quality switch is proposed; closing other programs helps a little.",
        "O mundo precisa de mais memória de vídeo do que muitas placas de 8 GB têm. Há uma opção de qualidade de textura menor proposta; fechar outros programas ajuda um pouco.",
    ),
    (
        "Windows ran out of commit (the paging file)",
        "O Windows ficou sem memória virtual (arquivo de paginação)",
    ),
    (
        "Close other applications or make the Windows paging file larger, then try again (see Getting started in the README).",
        "Feche outros programas ou aumente o arquivo de paginação do Windows e tente de novo (veja Getting started no README).",
    ),
    (
        "That file isn't a title's executable",
        "Esse arquivo não é o executável de um jogo",
    ),
    (
        "VIctory runs the title's own .exe from your copy of the game. Pick that file.",
        "O VIctory roda o .exe do próprio jogo, da sua cópia. Escolha esse arquivo.",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_phrase_is_one_the_menu_or_the_doctor_writes() {
        let sources = [include_str!("menu.rs"), include_str!("doctor.rs")].concat();
        // As the source spells it (escapes, and the line ends some keys hold).
        for (en, pt) in PT {
            let spelled = en.replace('\\', "\\\\").replace('\n', "\\n");
            // ("Data folder: " is "Data folder" and ": " put together.)
            let needle = spelled.trim_matches(|c: char| matches!(c, ' ' | '\\' | 'n' | ':'));
            assert!(sources.contains(needle), "not written anywhere: {en:?}");
            assert!(!pt.is_empty() && pt != en, "{en:?}");
        }
    }

    #[test]
    fn the_portuguese_menu_reads_whole() {
        let menu = "\n  V I C T O R Y\n  your own copy of the title, on this PC\n\n  1  Run a game\n  2  Run again   (nothing run yet)\n  3  Check this PC\n  S  Set the game folder   (not set)\n  M  Console: Series X   (M switches to Series S)\n  Q  Quit\n\n  Choose: ";
        let pt = to_portuguese(menu);
        for phrase in [
            "a sua cópia do jogo, neste PC",
            "1  Rodar um jogo",
            "2  Rodar de novo   (nada rodado ainda)",
            "3  Checar este PC",
            "S  Definir a pasta do jogo   (não definida)",
            "(M troca para Series S)",
            "Q  Sair",
            "Escolha:",
        ] {
            assert!(pt.contains(phrase), "{phrase}\n{pt}");
        }
        // Longest first: "Game folder: not set" is not cut by "Game folder: ".
        assert_eq!(
            to_portuguese("Game folder: not set"),
            "Pasta do jogo: não definida"
        );
        assert_eq!(
            to_portuguese("Game folder: D:\\Games\\Title (not there)"),
            "Pasta do jogo: D:\\Games\\Title (não existe)"
        );
    }

    #[test]
    fn the_tests_and_the_report_stay_in_english() {
        assert!(!portuguese());
        assert_eq!(localize("Run a game"), "Run a game");
    }
}
