#!/usr/bin/env python3
"""PC-Eye ölçüm seti için deterministik GTK4 test uygulaması.

Her sahne bilinen (ve koşucunun rastgele ürettiği) içerik gösterir; kullanıcı
etkileşiminin sonucunu `--out` JSON dosyasına yazar. Koşucu başarıyı bu dosyadan
okur, böylece ajanın "yaptım" demesine değil gerçek duruma bakılır.

Sahneler:
  buttons  AT-SPI ağacı olan düğmeler; tıklanan düğmenin etiketi yazılır.
  form     Metin girişi + onay kutusu + açılır liste + Gönder; alanlar yazılır.
  canvas   Cairo ile çizilmiş metin (AT-SPI yok): yalnız OCR/görüntüyle okunur.
  orders   Salt okunur sipariş tablosu (iki uygulama arası kopyalama kaynağı).
  settings Kenar çubuklu ayarlar (sayfa gezinme, anahtar, sayı kutusu, açılır liste) + Uygula.
  pidform  PID alanı + Kaydet (arka plan bilgisini GUI'ye taşıma).
"""

import argparse
import json
import os
import sys

import gi

gi.require_version("Gtk", "4.0")
from gi.repository import Gtk  # noqa: E402


def write_out(path, data):
    tmp = f"{path}.tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(data, f, ensure_ascii=False)
    os.replace(tmp, path)


def build_buttons(args, out):
    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
    header = Gtk.Label(label="Bir düğme seç")
    box.append(header)
    status = Gtk.Label(label="")
    grid = Gtk.FlowBox(max_children_per_line=3, selection_mode=Gtk.SelectionMode.NONE)

    def clicked(lab):
        # Gerçek uygulamalar gibi görünür geri bildirim: doğrulama bunu görebilmeli.
        status.set_label(f"Seçilen: {lab}")
        write_out(out, {"clicked": lab})

    for label in args.labels.split("|"):
        btn = Gtk.Button(label=label)
        btn.connect("clicked", lambda _b, lab=label: clicked(lab))
        grid.append(btn)
    box.append(status)
    box.append(grid)
    return box


def build_form(args, out):
    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
    box.append(Gtk.Label(label="Kayıt formu"))

    name = Gtk.Entry(placeholder_text="Ad Soyad")
    name.update_property([Gtk.AccessibleProperty.LABEL], ["Ad Soyad"])
    box.append(name)

    newsletter = Gtk.CheckButton(label="Bültene abone ol")
    box.append(newsletter)

    cities = args.cities.split("|")
    city = Gtk.DropDown.new_from_strings(cities)
    city.update_property([Gtk.AccessibleProperty.LABEL], ["Şehir"])
    box.append(city)

    # Gerçek formlar gönderimi onaylar; onay yokken ajan sonucu ekran görüntüsü ve günlüklerle
    # aramak zorunda kalıyordu (web formunda "Kaydedildi" zaten vardı).
    status = Gtk.Label(label="")

    def submit(_b):
        write_out(out, {
            "name": name.get_text(),
            "newsletter": newsletter.get_active(),
            "city": cities[city.get_selected()],
        })
        status.set_label("Kaydedildi")

    send = Gtk.Button(label="Gönder")
    send.connect("clicked", submit)
    box.append(send)
    box.append(status)
    return box


def build_canvas(args, _out):
    area = Gtk.DrawingArea(content_width=900, content_height=300)

    def draw(_a, cr, _w, _h):
        cr.set_source_rgb(1, 1, 1)
        cr.paint()
        cr.set_source_rgb(0.1, 0.1, 0.1)
        cr.select_font_face("Sans")
        cr.set_font_size(30)
        cr.move_to(40, 90)
        cr.show_text("Gizli kod:")
        cr.set_font_size(54)
        cr.move_to(40, 180)
        cr.show_text(args.text)

    area.set_draw_func(draw)
    return area


def build_orders(args, _out):
    """Salt okunur sipariş tablosu (iki uygulama arası kopyalama görevi için kaynak)."""
    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
    box.append(Gtk.Label(label="Siparişler", xalign=0))
    grid = Gtk.Grid(column_spacing=32, row_spacing=6)
    for c, h in enumerate(["Sipariş No", "Müşteri", "Tutar"]):
        grid.attach(Gtk.Label(label=h, xalign=0), c, 0, 1, 1)
    for r, row in enumerate(args.rows.split("|"), start=1):
        for c, cell in enumerate(row.split(";")):
            grid.attach(Gtk.Label(label=cell, xalign=0, selectable=True), c, r, 1, 1)
    box.append(grid)
    return box


def build_settings(args, out):
    """Kenar çubuklu ayarlar: sayfalar arası gezinme, anahtar, sayı kutusu, açılır liste."""
    themes = args.cities.split("|")
    stack = Gtk.Stack()
    sidebar = Gtk.StackSidebar(stack=stack)
    sidebar.set_size_request(180, -1)

    def page(*widgets):
        b = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        for w in widgets:
            b.append(w)
        return b

    def row(label, widget, name):
        widget.update_property([Gtk.AccessibleProperty.LABEL], [name])
        b = Gtk.Box(spacing=12)
        b.append(Gtk.Label(label=label, xalign=0, hexpand=True))
        b.append(widget)
        return b

    username = Gtk.Entry(text=args.text)
    stack.add_titled(page(row("Kullanıcı adı", username, "Kullanıcı adı")), "genel", "Genel")

    font = Gtk.SpinButton.new_with_range(8, 32, 1)
    font.set_value(11)
    theme = Gtk.DropDown.new_from_strings(themes)
    stack.add_titled(page(row("Yazı boyutu", font, "Yazı boyutu"), row("Tema", theme, "Tema")), "gorunum", "Görünüm")

    telemetry = Gtk.Switch(active=True, halign=Gtk.Align.END)
    cookies = Gtk.CheckButton(label="Çıkışta çerezleri temizle")
    stack.add_titled(page(row("Telemetri gönder", telemetry, "Telemetri gönder"), cookies), "gizlilik", "Gizlilik")

    status = Gtk.Label(label="")

    def apply(_b):
        write_out(out, {
            "username": username.get_text(),
            "font_size": str(int(font.get_value())),
            "theme": themes[theme.get_selected()],
            "telemetry": telemetry.get_active(),
            "clear_cookies": cookies.get_active(),
        })
        status.set_label("Ayarlar kaydedildi")

    apply_btn = Gtk.Button(label="Uygula")
    apply_btn.connect("clicked", apply)

    # Kenar çubuğu tüm satırları göstersin: aksi halde son sayfa satırı "Uygula"nın altında kalıp
    # tıklamayı ona kaptırıyordu (AT-SPI kırpılmış satırın konumunu yine de bildirir).
    body = Gtk.Box(spacing=16, vexpand=True)
    body.append(sidebar)
    stack.set_hexpand(True)
    body.append(stack)
    outer = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
    outer.append(body)
    outer.append(apply_btn)
    outer.append(status)
    return outer


def build_pidform(args, out):
    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
    box.append(Gtk.Label(label="Süreç kaydı", xalign=0))
    pid = Gtk.Entry(placeholder_text="PID")
    pid.update_property([Gtk.AccessibleProperty.LABEL], ["PID"])
    box.append(pid)
    status = Gtk.Label(label="")

    def save(_b):
        write_out(out, {"pid": pid.get_text().strip()})
        status.set_label("Kaydedildi")

    btn = Gtk.Button(label="Kaydet")
    btn.connect("clicked", save)
    box.append(btn)
    box.append(status)
    return box


SCENES = {
    "buttons": build_buttons,
    "form": build_form,
    "canvas": build_canvas,
    "orders": build_orders,
    "settings": build_settings,
    "pidform": build_pidform,
}


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--scene", choices=SCENES, required=True)
    p.add_argument("--out", required=True)
    p.add_argument("--title", default="PC-Eye Eval")
    p.add_argument("--labels", default="Tamam|İptal")
    p.add_argument("--cities", default="İstanbul|Ankara")
    p.add_argument("--text", default="")
    p.add_argument("--rows", default="")
    args = p.parse_args()

    app = Gtk.Application(application_id=None)

    def activate(a):
        win = Gtk.ApplicationWindow(application=a, title=args.title)
        win.set_default_size(960, 540)
        content = SCENES[args.scene](args, args.out)
        content.set_margin_top(24)
        content.set_margin_bottom(24)
        content.set_margin_start(24)
        content.set_margin_end(24)
        win.set_child(content)
        win.present()

    app.connect("activate", activate)
    sys.exit(app.run([sys.argv[0]]))


if __name__ == "__main__":
    main()
