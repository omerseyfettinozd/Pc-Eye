#!/usr/bin/env python3
"""PC-Eye ölçüm seti için deterministik GTK4 test uygulaması.

Her sahne bilinen (ve koşucunun rastgele ürettiği) içerik gösterir; kullanıcı
etkileşiminin sonucunu `--out` JSON dosyasına yazar. Koşucu başarıyı bu dosyadan
okur, böylece ajanın "yaptım" demesine değil gerçek duruma bakılır.

Sahneler:
  buttons  AT-SPI ağacı olan düğmeler; tıklanan düğmenin etiketi yazılır.
  form     Metin girişi + onay kutusu + açılır liste + Gönder; alanlar yazılır.
  canvas   Cairo ile çizilmiş metin (AT-SPI yok): yalnız OCR/görüntüyle okunur.
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

    def submit(_b):
        write_out(out, {
            "name": name.get_text(),
            "newsletter": newsletter.get_active(),
            "city": cities[city.get_selected()],
        })

    send = Gtk.Button(label="Gönder")
    send.connect("clicked", submit)
    box.append(send)
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


SCENES = {"buttons": build_buttons, "form": build_form, "canvas": build_canvas}


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--scene", choices=SCENES, required=True)
    p.add_argument("--out", required=True)
    p.add_argument("--title", default="PC-Eye Eval")
    p.add_argument("--labels", default="Tamam|İptal")
    p.add_argument("--cities", default="İstanbul|Ankara")
    p.add_argument("--text", default="")
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
