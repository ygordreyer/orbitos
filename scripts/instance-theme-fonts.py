"""Generate native weight faces from the trusted viewer font directory."""
import argparse
from pathlib import Path
from fontTools.ttLib import TTFont
from fontTools.varLib.instancer import instantiateVariableFont

parser = argparse.ArgumentParser()
parser.add_argument("source", type=Path)
args = parser.parse_args()
target = Path(__file__).resolve().parents[1] / "crates/common/src/theme/packs/fonts"
for family in ["Nunito", "Roboto"]:
    source = args.source / f"{family}-Variable.ttf"
    original = TTFont(source)
    licenses = []
    for name_id in [0, 13, 14]:
        value = original["name"].getDebugName(name_id)
        if value:
            licenses.append(value)
    ofl = (target / "OFL.txt").read_text().split("This Font Software", 1)[1]
    (target / f"{family}-LICENSE.txt").write_text(licenses[0] + "\n\nThis Font Software" + ofl)
    for weight in range(300, 901, 100):
        font = TTFont(source, recalcTimestamp=False)
        axes = {axis.axisTag: axis.defaultValue for axis in font["fvar"].axes}
        axes["wght"] = weight
        instantiateVariableFont(font, axes, inplace=True).save(target / f"{family}-{weight}.ttf")
