.PHONY: default clean png pdf

default: model.png model.pdf

clean:
	rm -f model.png model.pdf

pdf: model.pdf
png: model.png

model.pdf: model.dot
	./setdotlabelwidth 45 <model.dot | dot -Tpdf -o model.pdf

model.png: model.dot
	./setdotlabelwidth 45 <model.dot | dot -Tpng -o model.png
