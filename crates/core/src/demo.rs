//! Synthetic mail for M0. Every address uses `example.com` or `example.it`; no real person or
//! mailbox is represented. The data exists so the UI, CLI and MCP can be exercised before IMAP.

use chrono::{DateTime, Duration, Utc};

use crate::engine::Engine;
use crate::model::{Account, Address, Attachment, Folder, FolderKind, Message, MessageSummary};

const ACCOUNT_ID: &str = "acc_demo";

pub fn seed() -> Engine {
    let now = Utc::now();
    let accounts = vec![Account {
        id: ACCOUNT_ID.to_owned(),
        name: "Federico".to_owned(),
        email: "federico@example.it".to_owned(),
        provider: "Aruba · demo".to_owned(),
    }];
    let folders = vec![
        folder("inbox", "Posta in arrivo", FolderKind::Inbox),
        folder("starred", "Preferiti", FolderKind::Starred),
        folder("sent", "Inviati", FolderKind::Sent),
        folder("drafts", "Bozze", FolderKind::Drafts),
        folder("archive", "Archivio", FolderKind::Archive),
        folder("progetti", "Progetti", FolderKind::Custom),
        folder("spam", "Spam", FolderKind::Spam),
        folder("trash", "Cestino", FolderKind::Trash),
    ];

    let mut messages = Vec::new();

    let mut preventivo = message(
        "msg_001",
        "thr_preventivo",
        "inbox",
        ("Giulia Ferri", "giulia.ferri@example.com"),
        vec![("Federico", "federico@example.it")],
        "Preventivo sito web — bozza finale",
        now - Duration::minutes(8),
        true,
        true,
        "Ciao Federico,\n\nho rivisto il preventivo dopo la call di ieri. Ho tenuto la parte di identità visiva separata dallo sviluppo, così puoi decidere se partire prima con il logo o con il sito.\n\nIl totale è 4.800 € + IVA, consegna in sei settimane. Trovi il dettaglio nel PDF.\n\nFammi sapere se ti torna.\n\nGiulia",
    );
    attach(&mut preventivo, "att_001", "preventivo-postino.pdf", "application/pdf", 248_320);
    messages.push(preventivo);

    messages.push(message(
        "msg_002",
        "thr_preventivo",
        "sent",
        ("Federico", "federico@example.it"),
        vec![("Giulia Ferri", "giulia.ferri@example.com")],
        "Re: Preventivo sito web — bozza finale",
        now - Duration::minutes(24),
        false,
        false,
        "Ciao Giulia,\n\nil preventivo mi torna. Una cosa sola: vorrei capire se nella parte di identità è incluso anche il set per le email transazionali.\n\nGrazie,\nFederico",
    ));

    messages.push(message(
        "msg_003",
        "thr_preventivo",
        "inbox",
        ("Giulia Ferri", "giulia.ferri@example.com"),
        vec![("Federico", "federico@example.it")],
        "Re: Preventivo sito web — bozza finale",
        now - Duration::minutes(41),
        true,
        false,
        "Sì, è incluso. Ti mando il dettaglio aggiornato domani mattina.\n\nGiulia",
    ));

    let mut fattura = message(
        "msg_004",
        "thr_fattura",
        "inbox",
        ("Aruba S.p.A.", "fatture@aruba.example"),
        vec![("Federico", "federico@example.it")],
        "La tua fattura di ottobre",
        now - Duration::hours(3),
        true,
        false,
        "Ciao Federico,\n\nin allegato trovi la fattura di ottobre per i servizi di hosting e posta elettronica.\n\nLa ricevuta è disponibile anche nell'area clienti.\n\nAruba S.p.A.",
    );
    attach(&mut fattura, "att_002", "fattura-ottobre-2026.pdf", "application/pdf", 91_412);
    messages.push(fattura);

    let mut welcome = message(
        "msg_005",
        "thr_benvenuto",
        "inbox",
        ("Postino", "ciao@postino.example"),
        vec![("Federico", "federico@example.it")],
        "Benvenuto in Postino",
        now - Duration::hours(6),
        false,
        true,
        "Postino è il tuo client email locale, veloce e agent-first.\n\nQuesta è la modalità demo: i messaggi che vedi sono sintetici e nessun server è collegato. Serve per provare l'interfaccia, la CLI e il server MCP prima di configurare Aruba.\n\nQuando vuoi passare alle mail vere, il prossimo passo è l'account IMAP/SMTP.",
    );
    welcome.body_html =
        Some("<p>Postino è il tuo client email locale, veloce e agent-first.</p><p><strong>Questa è la modalità demo.</strong></p>".to_owned());
    messages.push(welcome);

    messages.push(message(
        "msg_006",
        "thr_pranzo",
        "inbox",
        ("Marco Bianchi", "marco.bianchi@example.com"),
        vec![("Federico", "federico@example.it")],
        "Pranzo sabato?",
        now - Duration::hours(9),
        false,
        false,
        "Ciao! Sabato sei libero per un pranzo veloce in centro? Pensavo all'una.\n\nMarco",
    ));

    let mut newsletter = message(
        "msg_007",
        "thr_newsletter",
        "inbox",
        ("ArtCraft", "news@artcraft.example"),
        vec![("Federico", "federico@example.it")],
        "ArtCraft — le novità di settembre",
        now - Duration::days(1) - Duration::hours(2),
        true,
        false,
        "Le novità di settembre\n\nQuesta è una newsletter sintetica con HTML. Nella modalità M0 il corpo viene mostrato come testo; il renderer HTML arriverà più avanti.\n\n• Nuovi strumenti per il colore\n• Miglioramenti al motore di rendering\n• Una community che cresce\n\nLeggi tutto sul sito.",
    );
    newsletter.body_html = Some(
        "<h1>Le novità di settembre</h1><p>Questa è una newsletter sintetica con HTML.</p><ul><li>Nuovi strumenti per il colore</li><li>Miglioramenti al motore di rendering</li></ul>"
            .to_owned(),
    );
    messages.push(newsletter);

    messages.push(message(
        "msg_008",
        "thr_riunione",
        "sent",
        ("Federico", "federico@example.it"),
        vec![("Marco Bianchi", "marco.bianchi@example.com"), ("Giulia Ferri", "giulia.ferri@example.com")],
        "Riunione di lunedì",
        now - Duration::days(1) - Duration::hours(3),
        false,
        false,
        "Ciao a tutti,\n\nconfermo la riunione di lunedì alle 10:00. Ordine del giorno: stato del progetto, prossimi passi e budget.\n\nA lunedì,\nFederico",
    ));

    messages.push(message(
        "msg_009",
        "thr_ordine",
        "inbox",
        ("Libreria Aurora", "ordini@libreria.example"),
        vec![("Federico", "federico@example.it")],
        "Ordine #2481 spedito",
        now - Duration::days(2),
        false,
        false,
        "Il tuo ordine #2481 è stato spedito.\n\nConsegna prevista: giovedì 29 ottobre. Puoi seguire il pacco dal link nella tua area clienti.\n\nGrazie per aver scelto Libreria Aurora.",
    ));

    messages.push(message(
        "msg_010",
        "thr_bozza",
        "drafts",
        ("Federico", "federico@example.it"),
        vec![("Studio Verde", "info@studioverde.example")],
        "Bozza: proposta di collaborazione",
        now - Duration::days(2) - Duration::hours(4),
        false,
        false,
        "Ciao,\n\nsto preparando una proposta di collaborazione per il nuovo progetto editoriale. Mi piacerebbe fissare una call la prossima settimana.\n\n[da completare: date, tariffa, allegati]",
    ));

    let mut contratto = message(
        "msg_011",
        "thr_contratto",
        "archive",
        ("Studio Legale Rossi", "segreteria@studiorossi.example"),
        vec![("Federico", "federico@example.it")],
        "Contratto di locazione 2026",
        now - Duration::days(9),
        false,
        true,
        "Buongiorno,\n\nin allegato la copia firmata del contratto di locazione per il 2026. Conservala per i tuoi archivi.\n\nCordiali saluti,\nStudio Legale Rossi",
    );
    attach(&mut contratto, "att_003", "contratto-locazione-2026.pdf", "application/pdf", 1_204_224);
    messages.push(contratto);

    messages.push(message(
        "msg_012",
        "thr_spam",
        "spam",
        ("Promo Vacanze", "vincite@promo.example"),
        vec![("Federico", "federico@example.it")],
        "Hai vinto un viaggio!",
        now - Duration::days(3),
        true,
        false,
        "Complimenti! Hai vinto un viaggio per due persone. Clicca qui per riscattare il premio entro 24 ore.\n\nQuesta è una mail sintetica di spam: non cliccare, non rispondere.",
    ));

    messages.push(message(
        "msg_013",
        "thr_newsletter_vecchia",
        "trash",
        ("Vecchia Newsletter", "news@vecchia.example"),
        vec![("Federico", "federico@example.it")],
        "Edizione arretrata",
        now - Duration::days(12),
        false,
        false,
        "Questa è una vecchia newsletter finita nel cestino.",
    ));

    let mut brief = message(
        "msg_014",
        "thr_brand",
        "progetti",
        ("Federico", "federico@example.it"),
        vec![("Federico", "federico@example.it")],
        "Brief — identità Postino",
        now - Duration::days(4),
        false,
        true,
        "Identità visiva di Postino\n\nPalette: verde bosco #173D32, avorio #F5EBD6, oro caldo #E4B96A. Tipografia Inter per l'interfaccia e JetBrains Mono per i metadati. Il logo è una busta con un timbro oro: deve restare leggibile a 16 px.\n\nDa fare: varianti colore, icona app, wordmark.",
    );
    attach(&mut brief, "att_004", "brief-brand.pdf", "application/pdf", 48_912);
    messages.push(brief);

    Engine::from_parts(accounts, folders, messages, true)
}

fn folder(id: &str, name: &str, kind: FolderKind) -> Folder {
    Folder { id: id.to_owned(), account_id: ACCOUNT_ID.to_owned(), name: name.to_owned(), kind, unread: 0, total: 0 }
}

#[allow(clippy::too_many_arguments)]
fn message(
    id: &str,
    thread: &str,
    folder: &str,
    from: (&str, &str),
    to: Vec<(&str, &str)>,
    subject: &str,
    date: DateTime<Utc>,
    unread: bool,
    starred: bool,
    body: &str,
) -> Message {
    let from = Address::new(from.0, from.1);
    let to = to.into_iter().map(|(name, email)| Address::new(name, email)).collect::<Vec<_>>();
    Message {
        summary: MessageSummary {
            id: id.to_owned(),
            thread_id: thread.to_owned(),
            folder_id: folder.to_owned(),
            from,
            to: to.clone(),
            subject: subject.to_owned(),
            snippet: snippet(body),
            date,
            unread,
            starred,
            has_attachments: false,
        },
        cc: Vec::new(),
        bcc: Vec::new(),
        body_text: body.to_owned(),
        body_html: None,
        attachments: Vec::new(),
    }
}

fn attach(message: &mut Message, id: &str, filename: &str, mime: &str, size: u64) {
    message.attachments.push(Attachment { id: id.to_owned(), filename: filename.to_owned(), mime: mime.to_owned(), size });
    message.summary.has_attachments = true;
}

fn snippet(body: &str) -> String {
    let line = body.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("");
    let mut chars = line.chars();
    let text = chars.by_ref().take(120).collect::<String>();
    if chars.next().is_some() { format!("{text}…") } else { text }
}
