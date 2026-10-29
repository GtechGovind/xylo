use super::*;

impl Xylo {
    pub(super) fn connectors_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut page = div()
            .id("connectors-scroll")
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .child(heading("Tools", "Connect your apps and tools to Xylo."));
        let mut transport = div().flex().gap_2();
        for value in ["stdio", "http"] {
            transport = transport.child(
                button(SharedString::from(format!("transport-{value}")), value)
                    .bg(rgb(if self.connector_transport == value {
                        lavender()
                    } else {
                        panel()
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.connector_transport = value.into();
                        cx.notify();
                    })),
            );
        }
        let mut form = card()
            .max_w(px(820.))
            .child(subtitle("NEW CONNECTOR"))
            .child(transport)
            .child(Input::new(&self.connector_name));
        if self.connector_transport == "stdio" {
            form = form
                .child(Input::new(&self.connector_command))
                .child(Input::new(&self.connector_args));
        } else {
            form = form.child(Input::new(&self.connector_endpoint));
        }
        page = page.child(
            form.child(
                primary("connector-save", "Register connector")
                    .on_click(cx.listener(|this, _, _, cx| this.save_connector(cx))),
            ),
        );
        for (i, connector) in self.connectors.iter().enumerate() {
            let id = connector.id;
            page = page.child(
                card()
                    .child(div().text_lg().child(connector.name.clone()))
                    .child(subtitle(format!(
                        "{} · {}",
                        connector.transport,
                        connector
                            .command
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| connector.endpoint.clone())
                    )))
                    .child(
                        button(("connector-tools", i), "Connect & list tools").on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.host_call(cx, |h| h.connector_tools(id))
                            }),
                        ),
                    ),
            );
        }
        for tool in &self.connector_tools {
            page = page.child(
                card()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(tool.name.clone()),
                    )
                    .child(subtitle(tool.description.clone())),
            );
        }
        page.child(self.native_tools_card(cx))
            .child(self.native_result_card(cx))
            .into_any_element()
    }
}
