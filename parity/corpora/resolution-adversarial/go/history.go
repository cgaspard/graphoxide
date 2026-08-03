package graphparity

type metricHistory struct {
	samples []int
}

func (h *metricHistory) append(value int) {
	h.samples = append(h.samples, value)
}

func record(h *metricHistory, value int) {
	h.append(value)
}
